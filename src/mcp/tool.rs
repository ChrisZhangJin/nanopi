//! Adapting an MCP tool to nanopi's `Tool` trait, and its multimodal
//! `CallToolResult` to the flat `ToolOutput`.
//!
//! Two halves, kept apart on purpose. [`to_tool_output`] is pure: a
//! total function from a parsed result to a `ToolOutput`, with no
//! transport in reach. Every interesting case there (a block type
//! nanopi cannot render, an empty result, a 4 MB wall of text) is a
//! property of the *payload*, and pinning those against a live server
//! would mean owning a server that can be made to emit them on demand.
//! [`McpTool`] is the other half: it owns the name the model sees and
//! the call into the client, and calls straight into the first.
//!
//! **The shapes do not line up, and the gap is lossy in one direction.**
//! `CallToolResult.content` is an ordered, heterogeneous array;
//! `ToolOutput` is one string plus a side-channel of images. Anything
//! that is neither text nor a model-renderable image has to become
//! *prose about itself*, because the alternative — dropping it — leaves
//! the model reasoning over a result it has no way to know is partial.
//! So every block produces at least one line of output. There is no
//! silent-drop path in this file.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::agent::context::ToolSpec;
use crate::mcp::client::{ClientError, McpClient};
use crate::mcp::protocol::{
    CallToolResult, ContentBlock, ResourceContents, Tool as McpToolDef,
};
use crate::tool::{
    ExecutionMode, ImageAttachment, Tool, ToolContext, ToolError, ToolOutput, ToolSource,
    MCP_TOOL_PREFIX,
};

// ------------------------------------------------------------- naming

/// Longest tool name both OpenAI and Anthropic accept
/// (`^[a-zA-Z0-9_-]{1,64}$`).
///
/// Enforced here rather than left to the provider because the provider's
/// answer is a 400 on the *whole* request: one over-long name from one
/// server would fail every turn for the rest of the session, including
/// turns that never touch MCP. `mcp__<server>__<tool>` overruns it
/// easily — a 30-character server name plus a verbose tool name is
/// enough.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Characters of hash appended to a shortened name. 8 hex digits is 32
/// bits: collisions need tens of thousands of tools sharing a prefix to
/// become likely, and `register_external` still refuses one that
/// happens anyway.
const HASH_LEN: usize = 8;

/// The name the model will call this tool by: `mcp__<server>__<tool>`,
/// restricted to what providers accept and at most
/// [`MAX_TOOL_NAME_LEN`] long.
///
/// **Lowercased as well as sanitised**, which the naming rule alone does
/// not ask for. The OpenAI provider lowercases every tool name at the
/// SSE source (`normalize_tool_name`, for gateways that return
/// `Bash_tool`), so a registered `mcp__GitHub__Search` could never be
/// matched: the model calls it, the provider hands the loop
/// `mcp__github__search`, and the registry — keyed by the mixed-case
/// name — reports "tool not found". Registering the lowercase form
/// makes that rewrite the identity for every MCP name. The cost is that
/// tools differing only in case collide; the manager reports the second
/// and keeps the first, which is the right outcome for names the
/// model's own gateway cannot tell apart.
///
/// **Shortening is deterministic and hash-suffixed.** A name that fits
/// is returned as is, so the common case reads naturally. One that does
/// not is cut to the budget and ends in `_<8 hex>` of a hash over the
/// *original* server and tool names. Truncation alone would map two
/// tools sharing a long prefix to one name; hashing the unsanitised
/// input also separates names that sanitising would merge (`a.b` and
/// `a_b`) once they are long enough to be cut. It is FNV-1a written out
/// here rather than `DefaultHasher`, whose output is explicitly not
/// stable across Rust releases — a name that changed with the toolchain
/// would orphan every session transcript that recorded it. The budget
/// is split so neither part can crowd the other out: each keeps up to
/// half, and a part shorter than that donates the rest.
pub fn registered_tool_name(server: &str, tool: &str) -> String {
    let (s, t) = (sanitise(server), sanitise(tool));
    let name = format!("{MCP_TOOL_PREFIX}{s}__{t}");
    if name.len() <= MAX_TOOL_NAME_LEN {
        return name;
    }

    // Both parts are ASCII after `sanitise`, so byte slicing below is
    // on character boundaries.
    let budget = MAX_TOOL_NAME_LEN - MCP_TOOL_PREFIX.len() - 2 - 1 - HASH_LEN;
    let half = budget / 2;
    let (s_take, t_take) = if s.len() <= half {
        (s.len(), budget - s.len())
    } else if t.len() <= half {
        (budget - t.len(), t.len())
    } else {
        (half, budget - half)
    };
    format!(
        "{MCP_TOOL_PREFIX}{}__{}_{:08x}",
        &s[..s_take],
        &t[..t_take],
        fnv1a32(server, tool)
    )
}

fn sanitise(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Low 32 bits of 64-bit FNV-1a over `server 0xFF tool`. `0xFF` never
/// occurs in UTF-8, so `("ab","c")` and `("a","bc")` cannot hash the
/// same bytes.
fn fnv1a32(server: &str, tool: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in server.bytes().chain(std::iter::once(0xff)).chain(tool.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as u32
}

/// A server-supplied `inputSchema` made safe to hand to a provider.
///
/// Providers require the top-level schema to be `type: "object"`, and a
/// rejection is — like an over-long name — a 400 on the whole request,
/// not on the one tool. The spec requires an object schema, but
/// `protocol.rs` parses leniently on purpose so one sloppy server does
/// not void a whole `tools/list`; this is where that leniency has to
/// stop. A missing `type` is filled in (the properties are usually
/// right); anything else non-object is replaced by the permissive
/// schema, since passing it on would only move the failure to the
/// provider.
pub(crate) fn parameters_schema(schema: Value) -> Value {
    match schema {
        Value::Object(mut m) => match m.get("type") {
            Some(Value::String(t)) if t == "object" => Value::Object(m),
            None => {
                m.insert("type".into(), json!("object"));
                Value::Object(m)
            }
            Some(_) => json!({ "type": "object" }),
        },
        _ => json!({ "type": "object" }),
    }
}

// --------------------------------------------------------------- tool

/// One MCP server tool, callable by the model.
///
/// Holds an `Arc<McpClient>` shared with every sibling tool of the same
/// server, which is what the client's `&self` API was shaped for:
/// concurrent calls multiplex over the one pipe.
pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    endpoint: String,
    /// The server's own name for the tool. Kept apart from `spec.name`
    /// because the two differ whenever sanitising, lowercasing or
    /// shortening changed anything, and `tools/call` must carry the
    /// server's: sending the registered name would be an "unknown tool"
    /// error for exactly the tools whose names had to be adjusted.
    remote_name: String,
    spec: ToolSpec,
    read_only: bool,
}

impl McpTool {
    /// `endpoint` is what `/tools` shows as the tool's origin. The
    /// caller builds it (see `manager::endpoint_of`) because it must
    /// never carry the server's `env`, and only the caller has the
    /// config.
    pub fn new(client: Arc<McpClient>, endpoint: impl Into<String>, def: McpToolDef) -> Self {
        let server = client.name().to_string();
        let description = def
            .description
            .filter(|d| !d.trim().is_empty())
            .or_else(|| def.title.filter(|t| !t.trim().is_empty()))
            .unwrap_or_else(|| format!("Tool \"{}\" from MCP server \"{server}\".", def.name));
        let read_only = def
            .annotations
            .as_ref()
            .and_then(|a| a.read_only_hint)
            .unwrap_or(false);
        Self {
            spec: ToolSpec {
                name: registered_tool_name(&server, &def.name),
                description,
                parameters: parameters_schema(def.input_schema),
            },
            remote_name: def.name,
            endpoint: endpoint.into(),
            server,
            client,
            read_only,
        }
    }

    /// The name `tools/call` carries.
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    /// The tool as the parent↔child channel shows it to a child agent:
    /// the *registered* name (the one a child asks for and the allowlist
    /// holds), the description and the schema, and nothing about where it
    /// came from. The endpoint and the server's `env` are deliberately
    /// not reachable from here — a child is model-driven and must not be
    /// able to learn how a server was launched or reached (a URL, a
    /// header).
    pub fn definition(&self) -> McpToolDef {
        McpToolDef {
            name: self.spec.name.clone(),
            title: None,
            description: Some(self.spec.description.clone()),
            input_schema: self.spec.parameters.clone(),
            output_schema: None,
            annotations: None,
        }
    }

    /// The call without the `ToolOutput` flattening, for the channel that
    /// must hand a child the same `CallToolResult` shape the parent got.
    pub async fn call_raw(
        &self,
        arguments: Map<String, Value>,
    ) -> Result<CallToolResult, ClientError> {
        self.client.call_tool(&self.remote_name, arguments).await
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn source(&self) -> ToolSource {
        ToolSource::Mcp { server: self.server.clone(), endpoint: self.endpoint.clone() }
    }

    /// **Sequential, unless the server declares `readOnlyHint`.**
    ///
    /// An MCP tool is as opaque as `bash`: nothing nanopi can inspect
    /// says what it touches, and `mutation_key` only understands `edit`
    /// and `write`. A filesystem server's `write_file` run beside
    /// another call on the same path loses an update, and — as with
    /// parallel `bash` — both calls report success, so the loss never
    /// reaches the model. Parallel is therefore the wrong default.
    ///
    /// Sequential is not free: it serialises the *whole* batch (see
    /// `ExecutionMode`), including unrelated `read`s, so a blanket
    /// default would tax every model that fans out three searches
    /// against one network server. `readOnlyHint` is the way out.
    /// `docs/mcp.md` rules the annotation out as a *security boundary*,
    /// and this is not one: nothing is granted or withheld on its
    /// strength, a server that lies only races its own calls, and every
    /// configured server already runs with the user's full privileges.
    /// A wrong hint costs a possible lost update; a missing one costs
    /// latency.
    fn execution_mode(&self) -> ExecutionMode {
        if self.read_only {
            ExecutionMode::Parallel
        } else {
            ExecutionMode::Sequential
        }
    }

    /// Every failure is an in-band `is_error` result, never `Err`: the
    /// model can read it and recover, whereas an `Err` is nanopi's own
    /// failure channel. This matches `WasmTool` and `tool/agent.rs`'s
    /// soft errors — a dead server must cost one tool call, not the
    /// turn.
    ///
    /// The ways a call can fail stay distinguishable in the message and
    /// in `metadata.mcp_error`, because they call for different
    /// reactions: `isError: true` is the tool's own answer, passed
    /// through untouched by [`to_tool_output`]; an RPC error means the
    /// server refused the request, so repeating it unchanged gets the
    /// same answer; a transport failure means the server may be gone,
    /// so retrying is pointless until it is back.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        // `null` is what a provider sends for a tool called with no
        // arguments; anything else non-object is a model error, refused
        // here rather than shipped to a server that would answer with a
        // less readable one.
        let arguments: Map<String, Value> = match args {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => {
                return Ok(failure(
                    "invalid_arguments",
                    format!(
                        "MCP tool \"{}\" takes a JSON object of arguments, got {}",
                        self.spec.name,
                        json_kind(&other)
                    ),
                    None,
                ));
            }
        };

        Ok(match self.client.call_tool(&self.remote_name, arguments).await {
            Ok(result) => to_tool_output(result),
            Err(ClientError::Rpc(e)) => failure(
                "rpc",
                format!(
                    "MCP server \"{}\" rejected the call to \"{}\": {}",
                    self.server,
                    self.remote_name,
                    elide(&e.to_string(), ERROR_DETAIL_BYTES)
                ),
                Some(e.code),
            ),
            // The message already names the server and carries only the
            // headline of its failure; the stderr tail stays out of the
            // model's context (see `ClientError::NotReady`).
            Err(e @ ClientError::NotReady { .. }) => {
                failure("not_ready", elide(&e.to_string(), ERROR_DETAIL_BYTES), None)
            }
            Err(ClientError::Transport(e)) => failure(
                "transport",
                format!(
                    "MCP server \"{}\" failed during the call to \"{}\": {}",
                    self.server,
                    self.remote_name,
                    elide(&e.to_string(), ERROR_DETAIL_BYTES)
                ),
                None,
            ),
            Err(ClientError::Protocol(m)) => failure(
                "protocol",
                format!(
                    "MCP server \"{}\" sent an unusable reply to \"{}\": {}",
                    self.server,
                    self.remote_name,
                    elide(&m, ERROR_DETAIL_BYTES)
                ),
                None,
            ),
        })
    }
}

/// How much of a server-supplied error string reaches the model. An RPC
/// message or a transport reason can embed a stack trace or a stderr
/// dump, and one failed call should not spend the context on it.
pub(crate) const ERROR_DETAIL_BYTES: usize = 1_000;

pub(crate) fn failure(kind: &str, message: String, code: Option<i32>) -> ToolOutput {
    let mut meta = json!({ "mcp_error": kind });
    if let Some(code) = code {
        meta["code"] = json!(code);
    }
    ToolOutput { content: message, is_error: true, metadata: Some(meta), images: Vec::new() }
}

pub(crate) fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// First line of `s`, control characters flattened and capped at `max`
/// characters — for status rows and startup notices.
///
/// Control characters are replaced rather than kept because the source
/// is a server's stderr, written to the user's terminal as-is: a server
/// that colours its logs would otherwise inject escape sequences into
/// nanopi's own output, and a hostile one could rewrite the screen.
pub fn first_line(s: &str, max: usize) -> String {
    let line: String = s
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(max).collect();
        out.push('…');
        out
    }
}

// ------------------------------------------------------ result mapping

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
pub(crate) fn elide(s: &str, max: usize) -> String {
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

    // ------------------------------------------------------- naming

    fn valid_provider_name(n: &str) -> bool {
        !n.is_empty()
            && n.len() <= 64
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn sanitising_replaces_every_character_a_provider_would_reject() {
        assert_eq!(registered_tool_name("my server", "do.thing/now"), "mcp__my_server__do_thing_now");
        // One underscore per *character*, not per byte: a CJK name must
        // not balloon past the limit on its encoded length.
        assert_eq!(registered_tool_name("服务", "查询"), "mcp________");
        assert!(valid_provider_name(&registered_tool_name("服务", "查询")));
        assert_eq!(registered_tool_name("gh", "search-code_v2"), "mcp__gh__search-code_v2");
    }

    /// The OpenAI provider lowercases tool names at the SSE source, so
    /// a mixed-case registered name could never be dispatched.
    #[test]
    fn registered_names_are_lowercase_so_the_provider_rewrite_is_the_identity() {
        let name = registered_tool_name("GitHub", "SearchCode");
        assert_eq!(name, "mcp__github__searchcode");
        assert_eq!(crate::tool::normalize_tool_name(&name), name);
    }

    #[test]
    fn a_name_exactly_at_the_limit_is_left_alone() {
        let tool = "t".repeat(MAX_TOOL_NAME_LEN - "mcp__s__".len());
        let name = registered_tool_name("s", &tool);
        assert_eq!(name.len(), MAX_TOOL_NAME_LEN);
        assert_eq!(name, format!("mcp__s__{tool}"), "no hash when it already fits");
    }

    #[test]
    fn an_over_long_name_is_cut_to_exactly_the_provider_limit() {
        for (s, t) in [
            ("s".to_string(), "t".repeat(200)),
            ("s".repeat(200), "t".to_string()),
            ("server".repeat(10), "tool".repeat(30)),
            ("x".to_string(), "日本語".repeat(40)),
        ] {
            let name = registered_tool_name(&s, &t);
            assert_eq!(name.len(), MAX_TOOL_NAME_LEN, "{name}");
            assert!(valid_provider_name(&name), "{name}");
            assert!(name.starts_with("mcp__"), "{name}");
        }
    }

    /// Neither half may be starved: a long server name still leaves
    /// room to tell its tools apart, and vice versa.
    #[test]
    fn a_long_part_cannot_crowd_the_other_out() {
        let long_server = registered_tool_name(&"s".repeat(100), "search");
        assert!(long_server.contains("__search_"), "{long_server}");
        let long_tool = registered_tool_name("github", &"t".repeat(100));
        assert!(long_tool.starts_with("mcp__github__t"), "{long_tool}");
    }

    #[test]
    fn shortening_is_deterministic_and_separates_shared_prefixes() {
        let a = registered_tool_name("srv", &format!("{}a", "x".repeat(80)));
        let b = registered_tool_name("srv", &format!("{}b", "x".repeat(80)));
        assert_eq!(a, registered_tool_name("srv", &format!("{}a", "x".repeat(80))));
        // Same 48-char prefix, differ only past the cut: the hash is the
        // only thing keeping them apart.
        assert_ne!(a, b);
    }

    /// Sanitising merges `a.b` and `a_b`; once the name is long enough
    /// to be shortened, the hash over the *original* strings still
    /// tells them apart, so the long form is collision-free where the
    /// short form is not.
    #[test]
    fn names_that_sanitise_alike_stay_distinct_once_shortened() {
        let pad = "p".repeat(80);
        assert_eq!(
            registered_tool_name("s", "a.b"),
            registered_tool_name("s", "a_b"),
            "short names merge, which is why the manager must report the second"
        );
        assert_ne!(
            registered_tool_name("s", &format!("a.b{pad}")),
            registered_tool_name("s", &format!("a_b{pad}"))
        );
    }

    #[test]
    fn a_name_is_never_reshaped_by_the_registrys_canonicalisation() {
        // `search_tool` is a real server tool name; M0's guarantee must
        // hold for the names this module actually produces.
        let name = registered_tool_name("github", "search_tool");
        let mut r = crate::tool::ToolRegistry::new();
        r.register_external(Arc::new(DeadTool(name.clone()))).unwrap();
        assert_eq!(r.canonical_name(&name).as_deref(), Some(name.as_str()));
    }

    /// Minimal `Tool` for registry-facing assertions.
    struct DeadTool(String);
    #[async_trait]
    impl Tool for DeadTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec { name: self.0.clone(), description: String::new(), parameters: json!({}) }
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> Result<ToolOutput, ToolError> {
            unreachable!()
        }
    }

    // ------------------------------------------------------ schema

    #[test]
    fn a_non_object_input_schema_cannot_reach_a_provider() {
        assert_eq!(parameters_schema(json!(true)), json!({"type": "object"}));
        assert_eq!(parameters_schema(json!({"type": "string"})), json!({"type": "object"}));
        assert_eq!(parameters_schema(json!({"type": ["object", "null"]})), json!({"type": "object"}));
        assert_eq!(
            parameters_schema(json!({"properties": {"q": {"type": "string"}}})),
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        );
        let ok = json!({"type": "object", "properties": {}});
        assert_eq!(parameters_schema(ok.clone()), ok);
    }

    #[test]
    fn first_line_is_one_capped_line_with_no_control_characters() {
        assert_eq!(first_line("boom\nsecond", 100), "boom");
        assert_eq!(first_line("\x1b[31mred\x1b[0m", 100), "[31mred [0m");
        assert_eq!(first_line(&"x".repeat(50), 10), format!("{}…", "x".repeat(10)));
        assert_eq!(first_line("", 10), "");
    }

    // ---------------------------------------------- against a server

    #[cfg(unix)]
    mod live {
        use super::*;
        use crate::mcp::config::ServerConfig;
        use crate::mcp::protocol::ToolAnnotations;
        use std::collections::BTreeMap;

        /// Answers the handshake, then `tools/call` by the tool's name:
        /// `echo` reports the name it was called with, `bad` is a tool
        /// that ran and failed (`isError`), `refuse` is a protocol
        /// error, `die` kills the server mid-call.
        const SERVER: &str = r#"
            while IFS= read -r line; do
                id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
                case "$line" in
                    *'"method":"initialize"'*)
                        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' "$id" ;;
                    *'"method":"tools/call"'*)
                        name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
                        case "$name" in
                            bad) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ENOENT"}],"isError":true}}\n' "$id" ;;
                            refuse) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"Unknown tool"}}\n' "$id" ;;
                            die) echo "kaboom: out of memory" >&2; exit 3 ;;
                            *) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"called %s"}]}}\n' "$id" "$name" ;;
                        esac ;;
                esac
            done
        "#;

        async fn client(script: &str) -> Arc<McpClient> {
            let c = McpClient::connect(&ServerConfig::stdio("fake", "sh", vec!["-c".to_string(), script.to_string()], BTreeMap::new()))
            .await;
            Arc::new(c)
        }

        fn def(name: &str) -> McpToolDef {
            McpToolDef {
                name: name.to_string(),
                title: None,
                description: None,
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: None,
            }
        }

        fn ctx() -> ToolContext {
            ToolContext { cwd: std::env::temp_dir() }
        }

        /// The server only knows `a.b`; the model only knows
        /// `mcp__fake__a_b`. The adapter is the translation between
        /// them, and sending the wrong one is an "unknown tool" for
        /// exactly the tools whose names had to change.
        #[tokio::test]
        async fn the_call_carries_the_servers_name_not_the_registered_one() {
            let tool = McpTool::new(client(SERVER).await, "sh", def("a.b"));
            assert_eq!(tool.spec().name, "mcp__fake__a_b");
            assert_eq!(tool.remote_name(), "a.b");
            let out = tool.execute(json!({}), &ctx()).await.unwrap();
            assert!(!out.is_error, "{out:?}");
            assert_eq!(out.content, "called a.b");
        }

        /// Same, for a name that had to be shortened.
        #[tokio::test]
        async fn a_shortened_tool_still_calls_the_full_remote_name() {
            let long = format!("t{}", "x".repeat(100));
            let tool = McpTool::new(client(SERVER).await, "sh", def(&long));
            assert_eq!(tool.spec().name.len(), MAX_TOOL_NAME_LEN);
            let out = tool.execute(Value::Null, &ctx()).await.unwrap();
            assert_eq!(out.content, format!("called {long}"));
        }

        #[tokio::test]
        async fn a_tool_that_ran_and_failed_is_passed_through_as_its_own_answer() {
            let tool = McpTool::new(client(SERVER).await, "sh", def("bad"));
            let out = tool.execute(json!({}), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert_eq!(out.content, "ENOENT");
            // The server's own result: no adapter tag, which is what
            // keeps it distinct from the protocol failure below.
            assert!(out.metadata.is_none(), "{:?}", out.metadata);
        }

        #[tokio::test]
        async fn a_protocol_error_is_an_in_band_failure_distinct_from_is_error() {
            let tool = McpTool::new(client(SERVER).await, "sh", def("refuse"));
            let out = tool.execute(json!({}), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert!(out.content.contains("rejected"), "{}", out.content);
            assert!(out.content.contains("Unknown tool"), "{}", out.content);
            assert!(out.content.contains("fake"), "must name the server: {}", out.content);
            let meta = out.metadata.expect("tagged");
            assert_eq!(meta["mcp_error"], "rpc");
            assert_eq!(meta["code"], -32602);
        }

        /// A server dying mid-call costs one tool result, not the turn,
        /// and the next call says the server is down rather than hanging.
        #[tokio::test]
        async fn a_server_that_dies_mid_call_is_an_in_band_failure_naming_it() {
            let client = client(SERVER).await;
            let die = McpTool::new(client.clone(), "sh", def("die"));
            let out = die.execute(json!({}), &ctx()).await.unwrap();
            assert!(out.is_error, "{out:?}");
            assert!(out.content.contains("fake"), "{}", out.content);
            assert_eq!(out.metadata.unwrap()["mcp_error"], "transport");

            let echo = McpTool::new(client, "sh", def("echo"));
            let out = echo.execute(json!({}), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert!(out.content.contains("not ready"), "{}", out.content);
            assert!(out.content.contains("\"fake\""), "{}", out.content);
            assert_eq!(out.metadata.unwrap()["mcp_error"], "not_ready");
            // The headline only: the stderr tail is for `/mcp`, not for
            // the model's context.
            assert!(!out.content.contains('\n'), "{}", out.content);
        }

        #[tokio::test]
        async fn a_client_that_never_started_fails_the_call_instead_of_panicking() {
            let c = Arc::new(
                McpClient::connect(&ServerConfig::stdio("ghost", "/definitely/not/a/real/binary", Vec::new(), BTreeMap::new()))
                .await,
            );
            let tool = McpTool::new(c, "ghost", def("anything"));
            let out = tool.execute(json!({}), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert!(out.content.contains("ghost"), "{}", out.content);
        }

        #[tokio::test]
        async fn non_object_arguments_are_refused_without_calling_the_server() {
            let tool = McpTool::new(client(SERVER).await, "sh", def("echo"));
            let out = tool.execute(json!([1, 2]), &ctx()).await.unwrap();
            assert!(out.is_error);
            assert!(out.content.contains("an array"), "{}", out.content);
        }

        /// Opaque by default, like `bash`; the server's own read-only
        /// declaration is the only thing that buys parallelism.
        #[tokio::test]
        async fn execution_mode_is_sequential_unless_declared_read_only() {
            let c = client(SERVER).await;
            let opaque = McpTool::new(c.clone(), "sh", def("w"));
            assert_eq!(opaque.execution_mode(), ExecutionMode::Sequential);

            let mut ro = def("r");
            ro.annotations = Some(ToolAnnotations { read_only_hint: Some(true), ..Default::default() });
            assert_eq!(McpTool::new(c.clone(), "sh", ro).execution_mode(), ExecutionMode::Parallel);

            let mut not_ro = def("n");
            not_ro.annotations = Some(ToolAnnotations { read_only_hint: Some(false), ..Default::default() });
            assert_eq!(McpTool::new(c, "sh", not_ro).execution_mode(), ExecutionMode::Sequential);
        }

        #[tokio::test]
        async fn source_names_the_server_and_endpoint() {
            let tool = McpTool::new(client(SERVER).await, "npx -y thing", def("t"));
            assert_eq!(
                tool.source(),
                ToolSource::Mcp { server: "fake".into(), endpoint: "npx -y thing".into() }
            );
        }
    }
}
