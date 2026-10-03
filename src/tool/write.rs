//! `write` tool — overwrites or creates a file with the given content.
//!
//! Refuses to write outside cwd (`tool::resolve_in_cwd`). `read` has no
//! such guard on purpose — it is the mutation that needs bounding, not
//! the reading. Creates parent dirs as needed, but only after the path
//! has been accepted.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Write content to a file, overwriting if it exists. Creates parent directories as needed.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path or path relative to cwd."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("path must be a string".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("content must be a string".into()))?;

        // Reject anything that resolves outside cwd. Checked before the
        // `create_dir_all` below — a rejected path must not leave
        // directories behind outside the tree on its way to being
        // refused.
        let abs = crate::tool::resolve_in_cwd(&ctx.cwd, path_str)
            .map_err(ToolError::Execution)?;

        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ToolError::Execution(format!("cannot create parent {}: {e}", parent.display()))
            })?;
        }

        write_no_follow(&abs, content)
            .map_err(|e| ToolError::Execution(format!("cannot write {}: {e}", abs.display())))?;

        Ok(ToolOutput {
            content: format!("wrote {} bytes to {}", content.len(), abs.display()),
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({"path": abs.display().to_string(), "bytes": content.len()})),
        })
    }
}

/// Open with `O_NOFOLLOW` and refuse a multiply-linked target.
///
/// `resolve_in_cwd` decides whether a path is inside the tree; this
/// decides that the thing finally opened is the thing that was checked.
/// Between the two there is a window, and it is not theoretical — tool
/// calls in one response run concurrently by default, so a `bash` call
/// swapping a component for a symlink races a `write` call in the same
/// batch. Measured at roughly 0.6% success over a few thousand attempts
/// before this guard.
///
/// `O_NOFOLLOW` closes the symlink half: if the final component became
/// a link after the check, the open fails instead of following it. The
/// hard-link half cannot be closed by path resolution at all — a hard
/// link is not a reference to a name, it is the same inode — so the
/// link count is checked instead, which is coarse but honest.
///
/// Neither is a complete answer. The complete answer is `openat2` with
/// `RESOLVE_BENEATH`, which is Linux 5.6+ and would abandon the older
/// kernels this project exists to support.
fn write_no_follow(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;

    // Deliberately NOT `truncate(true)`. Truncation happens at open,
    // which would empty the file before the link count could be
    // checked — destroying the very data the check exists to protect.
    // The file is truncated below, after it has been accepted.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Queried through the open descriptor, not the path, so this
        // cannot be raced the way a second `stat` could.
        let meta = f.metadata()?;
        if meta.nlink() > 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to write a file with multiple hard links: the same \
                 inode is reachable from outside the working directory",
            ));
        }
    }

    f.set_len(0)?;
    f.write_all(content.as_bytes())?;
    f.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-write-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn creates_new_file() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "out.txt", "content": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "hello"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn overwrites_existing() {
        let dir = tmp();
        std::fs::write(dir.join("x.txt"), "old").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "x.txt", "content": "new"}), &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x.txt")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        WriteTool
            .execute(json!({"path": "a/b/c.txt", "content": "x"}), &ctx)
            .await
            .unwrap();
        assert!(dir.join("a/b/c.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_absolute_outside_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool
            .execute(json!({"path": "/tmp/nope.txt", "content": "x"}), &ctx)
            .await;
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: the old guard compared raw paths, so an absolute
    /// path that is *textually* prefixed by cwd but climbs out of it
    /// with `..` was accepted and written.
    #[tokio::test]
    async fn rejects_absolute_traversal_out_of_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        // Unique per run: a fixed name would be satisfied by a
        // leftover from an earlier failing run, turning the assertion
        // below into a false pass — or, worse, a false failure.
        let name = format!("escaped-abs-{}.txt", crate::util::uuid::v7());
        let escape = dir.join("..").join(&name);
        let r = WriteTool
            .execute(
                json!({"path": escape.display().to_string(), "content": "x"}),
                &ctx,
            )
            .await;
        assert!(r.is_err(), "traversal via `..` must be refused");
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "nothing may be written outside cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: relative paths skipped the guard entirely — it only
    /// ran on the absolute branch — so this went straight through
    /// `cwd.join(..)` and wrote outside the tree.
    #[tokio::test]
    async fn rejects_relative_traversal_out_of_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let name = format!("escaped-rel-{}.txt", crate::util::uuid::v7());
        let r = WriteTool
            .execute(json!({"path": format!("../{name}"), "content": "x"}), &ctx)
            .await;
        assert!(r.is_err(), "relative `..` must be refused too");
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "nothing may be written outside cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlinked directory inside cwd pointing out of it is the case
    /// lexical normalization alone cannot see — the deepest existing
    /// ancestor gets canonicalized precisely to catch this.
    #[tokio::test]
    #[cfg(unix)]
    async fn rejects_write_through_symlinked_dir() {
        let dir = tmp();
        let outside = tmp();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool
            .execute(json!({"path": "link/pwned.txt", "content": "x"}), &ctx)
            .await;
        assert!(r.is_err(), "a symlink out of cwd must be refused");
        assert!(
            !outside.join("pwned.txt").exists(),
            "nothing may be written through the symlink"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Regression: a *dangling* symlink defeated the first version of
    /// this guard. `exists()` follows the link, so a broken one reads
    /// as "does not exist", the walk stepped over it, and the name was
    /// re-attached as an ordinary new component well inside cwd — then
    /// `fs::write` followed it on open(2) and the content landed
    /// outside. Distinct from the resolvable-symlink case above, which
    /// canonicalization already caught; this one needs the target to
    /// NOT exist, which is also why it creates the escape rather than
    /// just redirecting into one.
    ///
    /// Reachable without a shell: a checked-out repo containing
    /// `notes.md -> ~/.ssh/authorized_keys` is enough.
    #[tokio::test]
    #[cfg(unix)]
    async fn rejects_write_through_dangling_symlink() {
        let base = tmp();
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let outside = base.join("victim.txt");
        assert!(!outside.exists(), "target must not exist — that is the point");
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();

        let ctx = ToolContext { cwd: cwd.clone() };
        let r = WriteTool
            .execute(json!({"path": "link", "content": "pwned"}), &ctx)
            .await;

        let msg = match r {
            Err(e) => e.to_string(),
            Ok(o) => panic!("a dangling symlink out of cwd must be refused, got {o:?}"),
        };
        // The message has to name the symlink. "No such file or
        // directory" about a path whose directory exists made the model
        // relay a fabricated reason to the user in end-to-end testing.
        assert!(
            msg.contains("symlink"),
            "refusal must say why, got {msg:?}"
        );
        assert!(
            !outside.exists(),
            "the write escaped cwd through the dangling symlink"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The TOCTOU half. `resolve_in_cwd` says the path is inside the
    /// tree; between that answer and the open, the final component can
    /// become a symlink pointing out. Tool calls in one response run
    /// concurrently by default, so a `bash` call doing the swap races a
    /// `write` call in the same batch — measured at roughly 0.6% before
    /// `O_NOFOLLOW`. Simulated here by swapping after the guard would
    /// have run, which is the same state the race produces.
    #[tokio::test]
    #[cfg(unix)]
    async fn write_refuses_a_symlink_swapped_in_after_the_check() {
        let base = tmp();
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let outside = base.join("victim.txt");
        std::fs::write(&outside, "original").unwrap();

        // The guard accepts this: a plain file inside cwd.
        let target = cwd.join("f.txt");
        std::fs::write(&target, "innocent").unwrap();
        assert!(crate::tool::resolve_in_cwd(&cwd, "f.txt").is_ok());

        // The race lands here — same name, now a link pointing out.
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&outside, &target).unwrap();

        let ctx = ToolContext { cwd: cwd.clone() };
        let r = WriteTool
            .execute(json!({"path": "f.txt", "content": "pwned"}), &ctx)
            .await;

        assert!(r.is_err(), "O_NOFOLLOW must refuse the swapped-in symlink");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "original",
            "the write followed the symlink out of cwd"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A hard link is the same inode under another name, so no amount
    /// of path resolution can tell it apart from an ordinary file. The
    /// link count is the only signal available.
    #[tokio::test]
    #[cfg(unix)]
    async fn write_refuses_a_hard_link_to_outside() {
        let base = tmp();
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let outside = base.join("victim.txt");
        std::fs::write(&outside, "original").unwrap();
        std::fs::hard_link(&outside, cwd.join("inside.txt")).unwrap();

        let ctx = ToolContext { cwd: cwd.clone() };
        let r = WriteTool
            .execute(json!({"path": "inside.txt", "content": "pwned"}), &ctx)
            .await;

        assert!(r.is_err(), "a multiply-linked file must be refused");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "original",
            "the write reached the shared inode"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The guard must run before `create_dir_all`, or a refused path
    /// still litters directories outside the tree on its way out.
    #[tokio::test]
    async fn refusal_creates_no_directories_outside_cwd() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let name = format!("sibling-{}", crate::util::uuid::v7());
        let r = WriteTool
            .execute(
                json!({"path": format!("../{name}/deep/f.txt"), "content": "x"}),
                &ctx,
            )
            .await;
        assert!(r.is_err());
        assert!(
            !dir.parent().unwrap().join(&name).exists(),
            "a refused write must not create directories outside cwd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_path_arg_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = WriteTool.execute(json!({"content": "x"}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
