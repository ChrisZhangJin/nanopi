//! `read` tool — reads a file with optional offset/limit.
//!
//! Text files: returns content as UTF-8 lines with offset/limit slicing.
//!
//! Image files (PNG / JPEG / GIF / WebP, detected by magic bytes): base64-
//! encodes the raw bytes and attaches them as a multimodal image block on
//! the ToolOutput. The Anthropic adapter forwards these into the next
//! request as `{"type":"image","source":{"type":"base64",...}}`. Vision
//! gating (see `agent::thinking::supports_vision`) strips the image on
//! text-only models and leaves a placeholder message instead.
//!
//! Path resolution: relative paths resolve against `ctx.cwd`. Absolute
//! paths must be within cwd (security: prevents reading /etc/shadow).

use std::path::PathBuf;

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use crate::agent::context::ToolSpec;
use crate::tool::{ImageAttachment, Tool, ToolContext, ToolError, ToolOutput};

/// Ceiling on the RAW image size we'll accept before base64. Anthropic's
/// per-image limit is 5 MB encoded, so 3.5 MB raw ≈ 4.67 MB base64,
/// leaving headroom for the JSON envelope. Larger images error out
/// rather than getting truncated (silent truncation would produce a
/// corrupt image that the model would fail to decode).
const MAX_IMAGE_RAW_BYTES: usize = 3_500_000;

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "Read a file from disk. Text files return their content; PNG / JPEG / GIF / WebP images return a multimodal block for vision-capable models.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path or path relative to cwd."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Optional 0-based line offset (text files only; ignored for images)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Optional max number of lines to return (text files only; ignored for images)."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path must be a string".into()))?;
        let abs = resolve_path(&ctx.cwd, path_str)?;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().map(|n| n as usize);

        // Read raw bytes first so we can magic-byte-detect. Images
        // don't need to be UTF-8 valid, and treating them as text would
        // return garbage.
        let raw = std::fs::read(&abs)
            .map_err(|e| ToolError::Execution(format!("cannot read {}: {e}", abs.display())))?;

        if let Some(media_type) = crate::util::image_detect::detect_media_type(&raw) {
            if raw.len() > MAX_IMAGE_RAW_BYTES {
                return Err(ToolError::Execution(format!(
                    "image too large: {} bytes at {} (max {} bytes)",
                    raw.len(),
                    abs.display(),
                    MAX_IMAGE_RAW_BYTES
                )));
            }
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&raw);
            return Ok(ToolOutput {
                content: format!("Read image file [{}]", media_type),
                is_error: false,
                images: vec![ImageAttachment {
                    media_type: media_type.to_string(),
                    data_base64,
                }],
                metadata: Some(json!({
                    "path": abs.display().to_string(),
                    "media_type": media_type,
                    "bytes": raw.len(),
                })),
            });
        }

        // Not an image → decode as UTF-8 text.
        let content = String::from_utf8(raw).map_err(|_| {
            ToolError::Execution(format!(
                "cannot read {}: file is binary but not a supported image format",
                abs.display()
            ))
        })?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let start = offset.min(total);
        let end = limit.map(|n| (start + n).min(total)).unwrap_or(total);
        let slice: Vec<&str> = lines[start..end].to_vec();
        let out = if slice.is_empty() {
            String::new()
        } else {
            slice.join("\n") + "\n"
        };

        Ok(ToolOutput {
            content: out,
            is_error: false,
            images: Vec::new(),
            metadata: Some(
                json!({"path": abs.display().to_string(), "lines": total, "offset": start, "limit": limit}),
            ),
        })
    }
}

/// Resolve a (possibly relative) path against cwd.
///
/// v0.9.2: no cwd-escape guard. `read` is read-only; PI / Claude Code
/// both let the model read files anywhere (see PI's
/// `core/tools/path-utils.ts::resolveToCwd`). The prior guard broke
/// legitimate flows — most visibly, the `<available_skills>` block
/// gives absolute paths under `~/.nanopi/skills/`, and the model's
/// natural "read the SKILL.md" step failed with "path escapes cwd".
/// Bash could always read those paths anyway, so the guard was
/// security theater with a real UX cost.
///
/// The escape check still applies to the mutating `write` / `edit`
/// tools (see `tool/write.rs`, `tool/edit.rs`).
fn resolve_path(cwd: &std::path::Path, p: &str) -> Result<PathBuf, ToolError> {
    let candidate = if std::path::Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    match std::fs::canonicalize(&candidate) {
        Ok(p) => Ok(p),
        Err(_) => Ok(candidate), // may not exist yet; downstream will error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-read-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn reads_full_file() {
        let dir = tmp();
        std::fs::write(dir.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = ReadTool
            .execute(json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("line2"));
        assert!(!out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tmp();
        let body = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("lines.txt"), &body).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        // offset=2 (0-indexed → start at line3), limit=3 → lines 3,4,5
        let out = ReadTool
            .execute(json!({"path": "lines.txt", "offset": 2, "limit": 3}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("line3"));
        assert!(out.content.contains("line4"));
        assert!(out.content.contains("line5"));
        assert!(!out.content.contains("line6"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = ReadTool.execute(json!({"path": "nope.txt"}), &ctx).await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A PNG file (magic bytes + IHDR) is detected and returned as a
    /// base64 image attachment rather than being treated as text.
    #[tokio::test]
    async fn reads_png_as_image_attachment() {
        let dir = tmp();
        let mut png_bytes: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png_bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        png_bytes.extend_from_slice(b"IHDR");
        png_bytes.extend_from_slice(&[0; 13]); // fake IHDR body
        std::fs::write(dir.join("pic.png"), &png_bytes).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = ReadTool
            .execute(json!({"path": "pic.png"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("image/png"), "got {:?}", out.content);
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert!(!out.images[0].data_base64.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An image over the size cap errors out with a clear message
    /// rather than blindly base64-encoding and letting Anthropic 400.
    #[tokio::test]
    async fn rejects_image_over_size_cap() {
        let dir = tmp();
        // JPEG magic + huge zero body.
        let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        jpg.extend_from_slice(&vec![0u8; 4_000_000]);
        std::fs::write(dir.join("huge.jpg"), &jpg).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = ReadTool.execute(json!({"path": "huge.jpg"}), &ctx).await;
        assert!(r.is_err());
        let msg = format!("{:?}", r.err().unwrap());
        assert!(msg.to_lowercase().contains("image too large"), "got {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v0.9.2 relaxed the cwd sandbox on read to match PI / Claude
    /// Code, so the model can pull SKILL.md files under `~/.nanopi/`
    /// even when the session's cwd is a project directory. Absolute
    /// path pointing outside cwd now succeeds if the file is readable.
    #[tokio::test]
    async fn absolute_path_outside_cwd_is_allowed() {
        let cwd = tmp();
        let other = tmp();
        let outside = other.join("skill.md");
        std::fs::write(&outside, "---\nname: x\n---\nbody\n").unwrap();
        let ctx = ToolContext { cwd: cwd.clone() };
        let out = ReadTool
            .execute(json!({"path": outside.display().to_string()}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("body"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&other);
    }
}
