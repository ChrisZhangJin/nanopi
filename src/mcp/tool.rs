//! Adapting an MCP tool to nanopi's `Tool` trait, and its multimodal
//! `CallToolResult` to the flat `ToolOutput`.
//!
//! This half is pure: [`to_tool_output`] is a total function from a
//! parsed result to a `ToolOutput`, with no transport in reach. That is
//! deliberate — every interesting case here (a block type nanopi cannot
//! render, an empty result, a 4 MB wall of text) is a property of the
//! *payload*, and pinning those against a live server would mean owning
//! a server that can be made to emit them on demand. `impl Tool for
//! McpTool` lands with the client in M3 and calls straight into this.
//!
//! **The shapes do not line up, and the gap is lossy in one direction.**
//! `CallToolResult.content` is an ordered, heterogeneous array;
//! `ToolOutput` is one string plus a side-channel of images. Anything
//! that is neither text nor a model-renderable image has to become
//! *prose about itself*, because the alternative — dropping it — leaves
//! the model reasoning over a result it has no way to know is partial.
//! So every block produces at least one line of output. There is no
//! silent-drop path in this file.

use base64::Engine as _;

use crate::mcp::protocol::{CallToolResult, ContentBlock, ResourceContents};
use crate::tool::{ImageAttachment, ToolOutput};

/// Caps on the assembled text, matching `tool/bash.rs`'s 30 KB / 2000
/// lines rather than `tool/read.rs`'s 50 KB / 2000.
///
/// Both exist; `bash` is the right analogue. `read` pairs its larger cap
/// with `offset`/`limit` so the model can page through what was cut — an
/// MCP tool has no such contract, and `bash`'s case (opaque output from
/// a foreign process, one shot at it) is exactly this one.
///
/// What is *not* copied is `bash`'s overflow file. Spilling the full
/// result to the tmpdir would make this function do I/O, and it would be
/// the wrong trade besides: `bash`'s overflow is re-readable because the
/// model can `read` the path, but it would be re-reading an MCP payload
/// it cannot act on any better the second time.
const MAX_BYTES: usize = 30_000;
const MAX_LINES: usize = 2000;

/// Ceiling on one image's base64 length, from the same arithmetic as
/// `read`'s `MAX_IMAGE_RAW_BYTES`: Anthropic rejects a request carrying
/// more than ~5 MB of encoded image, so 4.67 MB is the usable edge.
///
/// `read` *errors* when a file is over the line. This cannot: the
/// oversized image arrived alongside text the tool call also produced,
/// and failing the whole result to refuse one block would throw away
/// output the server successfully computed. It degrades to a note
/// instead.
const MAX_IMAGE_BASE64: usize = 4_670_000;

/// Image media types the providers will actually accept, from
/// `util::image_detect`'s canonical set.
///
/// Filtered rather than passed through, because `ImageAttachment`
/// media_type goes onto the wire verbatim: a server answering with
/// `image/svg+xml` or `image/bmp` — both legal MCP, neither accepted by
/// Anthropic — would turn one tool call into a 400 that kills the whole
/// turn. A degraded note costs the picture; the alternative costs the
/// conversation.
const RENDERABLE_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Flatten an MCP tool result into the shape nanopi's agent loop reads.
///
/// Takes the result by value: `content` holds base64 image payloads that
/// can run to megabytes, and a borrow here would mean cloning every one
/// of them into the `ImageAttachment`.
pub fn to_tool_output(result: CallToolResult) -> ToolOutput {
    let CallToolResult { content, structured_content, is_error } = result;

    let mut segments: Vec<String> = Vec::new();
    let mut images: Vec<ImageAttachment> = Vec::new();

    for block in content {
        segments.push(render_block(block, &mut images));
    }

    // Only when nothing readable came out of `content`. A server that
    // follows the spec's backward-compatibility advice sends its
    // structured result *twice* — once as JSON, once serialised into a
    // text block — and inlining unconditionally would show the model
    // both copies and invite it to treat them as two findings.
    //
    // The case this does cover is real and otherwise silent:
    // `metadata` is not part of what `agent::context` sends to the
    // model (see `ContextMessage::Tool`, which carries content,
    // is_error and images and nothing else), so a result that is
    // *only* `structuredContent` would reach the model as an empty
    // string.
    if segments.is_empty() {
        if let Some(value) = &structured_content {
            segments.push(format!(
                "[structured output]\n{}",
                serde_json::to_string_pretty(value)
                    .unwrap_or_else(|_| value.to_string())
            ));
        }
    }

    // Blank line, not newline. A text block is a document, not a line:
    // it is routinely multi-line and routinely ends without a trailing
    // newline, so joining on `\n` welds the last line of one block to
    // the first line of the next and loses the boundary the server drew.
    // It also breaks markdown, where a paragraph or list only ends at a
    // blank line.
    //
    // This is also why the non-text blocks above each render a visible
    // marker rather than contributing nothing: a `[text, image, text]`
    // result read as plain text has to show that something sat between
    // the two texts. Dropping the image from the string would present
    // them as adjacent prose, which is a claim about the result that the
    // server never made.
    let joined = segments.join("\n\n");

    let content = if joined.is_empty() {
        // An empty `content` with `isError: false` is a legal result —
        // plenty of tools are called for their effect. The empty string
        // is the one answer that cannot be given here: the model cannot
        // distinguish "it worked and said nothing" from "the adapter
        // lost the output", and in the second reading the sensible move
        // is to call the tool again. Say which one it was.
        "(tool returned no content)".to_string()
    } else {
        cap(joined)
    };

    ToolOutput { content, is_error, metadata: structured_content, images }
}

/// Render one block to text, pushing any model-renderable image onto
/// `images` as a side effect.
///
/// Every arm returns a non-empty string. See the module docs: the
/// marker is what keeps the result's ordering legible once the array has
/// been flattened into one string.
fn render_block(block: ContentBlock, images: &mut Vec<ImageAttachment>) -> String {
    match block {
        ContentBlock::Text { text, .. } => text,

        ContentBlock::Image { data, mime_type, .. } => {
            let media_type = RENDERABLE_IMAGE_TYPES
                .iter()
                .find(|t| t.eq_ignore_ascii_case(&mime_type));
            match media_type {
                Some(media_type) if data.len() <= MAX_IMAGE_BASE64 => {
                    // Decoded length, not base64 length: the number the
                    // model is shown should be the size of the picture,
                    // not of its transport encoding.
                    let bytes = decoded_len(&data);
                    images.push(ImageAttachment {
                        media_type: (*media_type).to_string(),
                        data_base64: data,
                    });
                    format!("[image: {media_type}, {bytes} bytes]")
                }
                Some(_) => format!(
                    "[image omitted: {mime_type}, {} bytes encoded — over the {} byte \
                     per-image limit]",
                    data.len(),
                    MAX_IMAGE_BASE64
                ),
                None => format!(
                    "[image omitted: {mime_type} is not a media type this model accepts]"
                ),
            }
        }

        // nanopi has no audio channel to the model — no `ToolOutput`
        // field carries it and no provider adapter would emit it. The
        // note is the whole mapping, and it has to exist: a transcription
        // tool whose audio vanished silently would read as a tool that
        // returned nothing.
        ContentBlock::Audio { data, mime_type, .. } => {
            format!(
                "[audio omitted: {mime_type}, {} bytes — nanopi cannot send audio to the model]",
                decoded_len(&data)
            )
        }

        // A pointer, not a payload. nanopi implements no `resources/*`
        // calls, so there is nothing to dereference it with; the uri is
        // still worth surfacing because the model may be able to reach
        // it by other means (a `read`, a `bash` curl).
        ContentBlock::ResourceLink { uri, name, title, description, mime_type, .. } => {
            let mut s = format!("[resource link: {name} <{uri}>");
            if let Some(mime) = &mime_type {
                s.push_str(&format!(", {mime}"));
            }
            s.push(']');
            if let Some(title) = title.filter(|t| *t != name) {
                s.push_str(&format!("\n{title}"));
            }
            if let Some(description) = description {
                s.push_str(&format!("\n{description}"));
            }
            s
        }

        ContentBlock::Resource { resource, .. } => render_resource(resource),

        // The arm this enum exists for. An unrecognised block is a
        // content type added between spec revisions, and the model is a
        // better judge of what to do about it than this function is —
        // but only if it is told. Printing the raw JSON (capped) beats
        // the bare type name: a block nanopi has no arm for may still be
        // one the model can read.
        ContentBlock::Unknown(value) => {
            let kind = value.get("type").and_then(serde_json::Value::as_str).unwrap_or("unknown");
            let body = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
            format!("[unsupported content: {kind}]\n{}", elide(&body, UNKNOWN_BLOCK_BYTES))
        }
    }
}

/// How much of an unrecognised block's raw JSON to show. Small on
/// purpose: a single unreadable block must not be able to crowd out the
/// text blocks beside it, and the point of printing it at all is that
/// the model can see *what* came back, not read it end to end.
const UNKNOWN_BLOCK_BYTES: usize = 2_000;

/// An embedded resource: the server already inlined the body, so unlike
/// a `resource_link` there is something to show.
fn render_resource(resource: ResourceContents) -> String {
    let ResourceContents { uri, name, title, mime_type, text, blob } = resource;
    let label = name.or(title).unwrap_or_else(|| uri.clone());
    let mime = mime_type.map(|m| format!(", {m}")).unwrap_or_default();

    // `text` wins when both are set. `ResourceContents` allows both
    // because a malformed server can send both (see its doc comment),
    // and in that case the readable half is the better answer.
    if let Some(text) = text {
        format!("[resource: {label} <{uri}>{mime}]\n{text}")
    } else if let Some(blob) = blob {
        // Not decoded and not attached: a resource blob carries no
        // guarantee of being an image, and base64 of an arbitrary
        // binary pasted into the content string is tokens spent on
        // noise. The size is the useful part — it tells the model
        // whether the thing it asked for actually exists.
        format!(
            "[resource: {label} <{uri}>{mime}, binary, {} bytes]",
            decoded_len(&blob)
        )
    } else {
        // Neither field set. Out of spec, but `ResourceContents` parses
        // it rather than failing the surrounding result, so it reaches
        // here and must say something.
        format!("[resource: {label} <{uri}>{mime}, empty]")
    }
}

/// Decoded byte count of base64 `data`, without allocating the decoded
/// bytes — these run to megabytes and the only use is a number in a
/// message. Falls back to the encoded length when the server's base64
/// does not parse, which is a wrong number but a better one than
/// pretending the field was absent.
fn decoded_len(data: &str) -> usize {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map(|b| b.len())
        .unwrap_or(data.len())
}

/// Lines-then-bytes cap, in that order, same as `tool/bash.rs`.
fn cap(text: String) -> String {
    let mut lines = text.lines();
    let head: Vec<&str> = lines.by_ref().take(MAX_LINES).collect();
    let line_capped = lines.next().is_some();

    // Untouched when nothing was dropped: `lines()` eats the trailing
    // newline, so rejoining unconditionally would silently edit a result
    // that fit. `bash.rs` has the same guard and the bug report to go
    // with it.
    let mut out = if line_capped { head.join("\n") } else { text };

    let byte_capped = out.len() > MAX_BYTES;
    if byte_capped {
        truncate_on_boundary(&mut out, MAX_BYTES);
    }

    if line_capped || byte_capped {
        out.push_str("\n[output truncated]");
    }
    out
}

/// `elide` and `cap` both cut a `String` to a byte budget; both would
/// panic on a multi-byte character, and MCP results are as likely to be
/// CJK or emoji as ASCII.
fn truncate_on_boundary(s: &mut String, max: usize) {
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// `cap` for a single block, with an inline marker instead of a trailing
/// one — the next block's text follows immediately, so a bare
/// `[output truncated]` would read as applying to the wrong thing.
fn elide(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s.to_string();
    truncate_on_boundary(&mut out, max);
    out.push_str("… [truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text { text: s.to_string(), annotations: None }
    }

    /// A one-pixel PNG, base64. Real bytes rather than `"AAAA"` so
    /// `decoded_len` is exercised on input that actually decodes.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn image(mime: &str) -> ContentBlock {
        ContentBlock::Image {
            data: PNG_B64.to_string(),
            mime_type: mime.to_string(),
            annotations: None,
        }
    }

    fn result(content: Vec<ContentBlock>) -> CallToolResult {
        CallToolResult { content, structured_content: None, is_error: false }
    }

    #[test]
    fn text_only_passes_through_verbatim() {
        let out = to_tool_output(result(vec![text("hello world")]));
        assert_eq!(out.content, "hello world");
        assert!(!out.is_error);
        assert!(out.images.is_empty());
        assert!(out.metadata.is_none());
    }

    /// The join is a BLANK line: two text blocks are two documents, and
    /// `\n` alone would weld the last line of one to the first of the
    /// next.
    #[test]
    fn two_text_blocks_are_separated_by_a_blank_line() {
        let out = to_tool_output(result(vec![text("first\nstill first"), text("second")]));
        assert_eq!(out.content, "first\nstill first\n\nsecond");
    }

    #[test]
    fn image_only_becomes_an_attachment_plus_a_marker() {
        let out = to_tool_output(result(vec![image("image/png")]));
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!(out.images[0].data_base64, PNG_B64);
        // Not the empty string: the model must be told an image came
        // back, because a text-only provider drops the attachment.
        assert!(out.content.starts_with("[image: image/png,"), "{:?}", out.content);
    }

    /// The ordering case. `[text, image, text]` flattened must not read
    /// as one two-paragraph document — the server put something between
    /// them.
    #[test]
    fn a_mixed_array_keeps_its_order_visible() {
        let out = to_tool_output(result(vec![text("before"), image("image/png"), text("after")]));
        assert_eq!(out.images.len(), 1);
        let before = out.content.find("before").unwrap();
        let marker = out.content.find("[image:").unwrap();
        let after = out.content.find("after").unwrap();
        assert!(before < marker && marker < after, "{:?}", out.content);
    }

    /// A media type no provider accepts is dropped from `images` rather
    /// than forwarded — forwarding it 400s the request and kills the
    /// turn — but it is still named in the text.
    #[test]
    fn an_unrenderable_image_type_degrades_to_a_note() {
        let out = to_tool_output(result(vec![image("image/svg+xml")]));
        assert!(out.images.is_empty());
        assert!(out.content.contains("image/svg+xml"), "{:?}", out.content);
        assert!(out.content.contains("omitted"), "{:?}", out.content);
        assert!(!out.is_error, "an unrenderable block is not a tool failure");
    }

    #[test]
    fn an_oversized_image_degrades_to_a_note() {
        let out = to_tool_output(result(vec![ContentBlock::Image {
            data: "A".repeat(MAX_IMAGE_BASE64 + 1),
            mime_type: "image/png".into(),
            annotations: None,
        }]));
        assert!(out.images.is_empty());
        assert!(out.content.contains("over the"), "{:?}", out.content);
    }

    /// Legal MCP: a tool called for its effect. The empty string is the
    /// one answer that cannot be given — it is indistinguishable from a
    /// bug in this adapter.
    #[test]
    fn an_empty_content_array_says_so() {
        let out = to_tool_output(result(vec![]));
        assert_eq!(out.content, "(tool returned no content)");
        assert!(!out.is_error);
        assert!(out.images.is_empty());
    }

    #[test]
    fn is_error_is_carried_through_with_the_text() {
        let out = to_tool_output(CallToolResult {
            content: vec![text("ENOENT: no such file")],
            structured_content: None,
            is_error: true,
        });
        assert!(out.is_error);
        assert_eq!(out.content, "ENOENT: no such file");
    }

    /// `structuredContent` goes to `metadata`, and does NOT get spliced
    /// into the text when the server already sent a readable block —
    /// the spec's backward-compat advice is to send both, and the model
    /// should not see one result twice.
    #[test]
    fn structured_content_becomes_metadata_and_does_not_duplicate_text() {
        let out = to_tool_output(CallToolResult {
            content: vec![text(r#"{"temp":22}"#)],
            structured_content: Some(json!({"temp": 22})),
            is_error: false,
        });
        assert_eq!(out.metadata, Some(json!({"temp": 22})));
        assert_eq!(out.content, r#"{"temp":22}"#);
    }

    /// `metadata` is not forwarded to the model by `agent::context`, so
    /// a structured-only result would otherwise arrive as an empty
    /// string. This is the one case where it IS inlined.
    #[test]
    fn structured_content_alone_is_inlined_as_text() {
        let out = to_tool_output(CallToolResult {
            content: vec![],
            structured_content: Some(json!({"temp": 22})),
            is_error: false,
        });
        assert_eq!(out.metadata, Some(json!({"temp": 22})));
        assert!(out.content.contains("\"temp\""), "{:?}", out.content);
        assert_ne!(out.content, "(tool returned no content)");
    }

    /// A block type from a spec revision nanopi has never seen. It must
    /// not vanish: the model reasoning over a result it cannot tell is
    /// partial is worse than the model seeing JSON it cannot parse.
    #[test]
    fn an_unknown_block_is_surfaced_with_its_type_and_body() {
        let out = to_tool_output(result(vec![
            text("here you go"),
            ContentBlock::Unknown(json!({"type": "video", "url": "https://x/y.mp4"})),
        ]));
        assert!(out.content.contains("here you go"));
        assert!(out.content.contains("[unsupported content: video]"), "{:?}", out.content);
        assert!(out.content.contains("y.mp4"), "{:?}", out.content);
    }

    /// `kind()` falls back to "unknown" for a block with no `type` at
    /// all, and so must the rendered marker.
    #[test]
    fn an_unknown_block_without_a_type_still_renders() {
        let out = to_tool_output(result(vec![ContentBlock::Unknown(json!({"shrug": true}))]));
        assert!(out.content.contains("[unsupported content: unknown]"), "{:?}", out.content);
    }

    #[test]
    fn an_embedded_resource_with_text_is_inlined() {
        let out = to_tool_output(result(vec![ContentBlock::Resource {
            resource: ResourceContents {
                uri: "file:///tmp/a.rs".into(),
                name: Some("a.rs".into()),
                title: None,
                mime_type: Some("text/x-rust".into()),
                text: Some("fn main() {}".into()),
                blob: None,
            },
            annotations: None,
        }]));
        assert!(out.content.contains("fn main() {}"), "{:?}", out.content);
        assert!(out.content.contains("file:///tmp/a.rs"), "{:?}", out.content);
    }

    /// A blob is summarised, never inlined: base64 of arbitrary binary
    /// is tokens spent on noise, and nothing says it is an image.
    #[test]
    fn an_embedded_resource_with_a_blob_is_summarised_not_inlined() {
        let out = to_tool_output(result(vec![ContentBlock::Resource {
            resource: ResourceContents {
                uri: "file:///tmp/a.bin".into(),
                name: None,
                title: None,
                mime_type: Some("application/octet-stream".into()),
                text: None,
                blob: Some(PNG_B64.into()),
            },
            annotations: None,
        }]));
        assert!(!out.content.contains(PNG_B64), "blob must not be inlined: {:?}", out.content);
        assert!(out.content.contains("binary"), "{:?}", out.content);
        assert!(out.content.contains("file:///tmp/a.bin"), "{:?}", out.content);
        assert!(out.images.is_empty(), "a blob is not an image attachment");
    }

    #[test]
    fn a_resource_with_neither_text_nor_blob_still_renders() {
        let out = to_tool_output(result(vec![ContentBlock::Resource {
            resource: ResourceContents {
                uri: "file:///tmp/gone".into(),
                name: None,
                title: None,
                mime_type: None,
                text: None,
                blob: None,
            },
            annotations: None,
        }]));
        assert!(out.content.contains("empty"), "{:?}", out.content);
    }

    #[test]
    fn a_resource_link_summarises_uri_name_and_mime() {
        let out = to_tool_output(result(vec![ContentBlock::ResourceLink {
            uri: "https://example.com/report.pdf".into(),
            name: "report.pdf".into(),
            title: Some("Q3 report".into()),
            description: Some("the quarterly numbers".into()),
            mime_type: Some("application/pdf".into()),
            annotations: None,
        }]));
        assert!(out.content.contains("https://example.com/report.pdf"), "{:?}", out.content);
        assert!(out.content.contains("report.pdf"), "{:?}", out.content);
        assert!(out.content.contains("application/pdf"), "{:?}", out.content);
        assert!(out.content.contains("the quarterly numbers"), "{:?}", out.content);
    }

    /// Audio has no field on `ToolOutput` and no provider adapter. The
    /// note is the whole mapping — dropping it would make a
    /// transcription tool look like it returned nothing.
    #[test]
    fn audio_degrades_to_a_note_rather_than_vanishing() {
        let out = to_tool_output(result(vec![ContentBlock::Audio {
            data: PNG_B64.into(),
            mime_type: "audio/wav".into(),
            annotations: None,
        }]));
        assert!(out.content.contains("audio/wav"), "{:?}", out.content);
        assert!(out.content.contains("omitted"), "{:?}", out.content);
        assert!(!out.is_error);
    }

    #[test]
    fn oversized_text_is_capped_by_bytes_and_marked() {
        let out = to_tool_output(result(vec![text(&"x".repeat(MAX_BYTES + 5_000))]));
        assert!(out.content.ends_with("[output truncated]"), "missing marker");
        assert!(out.content.len() < MAX_BYTES + 100, "len {}", out.content.len());
    }

    #[test]
    fn oversized_text_is_capped_by_lines_and_marked() {
        let body = (0..MAX_LINES + 500).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let out = to_tool_output(result(vec![text(&body)]));
        assert!(out.content.ends_with("[output truncated]"));
        assert!(out.content.contains(&format!("\n{}", MAX_LINES - 1)));
        assert!(!out.content.contains(&format!("\n{}\n", MAX_LINES)), "cap did not fire");
    }

    /// `String::truncate` panics when the index splits a character, so
    /// a multibyte result over the byte cap would take down the agent
    /// rather than being shortened. Same bug class as `bash.rs`'s.
    #[test]
    fn a_multibyte_result_over_the_cap_does_not_panic() {
        let out = to_tool_output(result(vec![text(&"日".repeat(MAX_BYTES))]));
        assert!(out.content.ends_with("[output truncated]"));
    }

    /// Output that fits is returned byte for byte — including the
    /// trailing newline `lines()` would have eaten.
    #[test]
    fn output_under_the_cap_is_untouched() {
        let out = to_tool_output(result(vec![text("a\nb\n")]));
        assert_eq!(out.content, "a\nb\n");
    }

    /// The whole `CallToolResult` from the spec's own `tools/call`
    /// example, parsed rather than hand-built, so the mapping is tested
    /// against the wire shape and not against our constructors.
    #[test]
    fn maps_the_spec_example_payload() {
        let parsed: CallToolResult = serde_json::from_value(json!({
            "content": [{
                "type": "text",
                "text": "Current weather in New York:\nTemperature: 72°F\nConditions: Partly cloudy"
            }],
            "isError": false
        }))
        .expect("spec payload must parse");
        let out = to_tool_output(parsed);
        assert!(out.content.starts_with("Current weather in New York:"));
        assert!(!out.is_error);
    }
}
