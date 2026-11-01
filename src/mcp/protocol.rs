//! The MCP wire format, revision `2025-06-18`: the JSON-RPC 2.0
//! envelope plus the `initialize` / `tools/*` payloads.
//!
//! Pure types and serde. No transport, no process spawning — `stdio.rs`
//! (M2) and `client.rs` (M3) own those, and keeping them out means this
//! file is testable against payloads pasted straight from the spec.
//!
//! **Two rules shape almost every decision here.**
//!
//! *A server's output is untrusted input.* Servers are third-party
//! subprocesses that ship on their own schedule, and the spec itself
//! keeps growing new content-block types. So the parse is lenient
//! wherever leniency degrades gracefully: an unrecognised content block
//! becomes [`ContentBlock::Unknown`] rather than failing the tool call
//! that carried it, and unknown capability keys are kept rather than
//! rejected. It is strict only where a wrong guess would be silently
//! acted on.
//!
//! *A protocol error is not a tool failure.* See [`ResponsePayload`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The revision this client speaks. Sent in `initialize` and, for the
/// HTTP transport someday, in the `MCP-Protocol-Version` header. A
/// server that answers with a different string has not agreed to it —
/// the handshake in M3 is what decides whether that is fatal.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Method names, as constants rather than literals at the call site:
/// `notifications/initialized` is easy to mistype as `initialized`, and
/// a server answers that mistake with silence rather than an error.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const CANCELLED: &str = "notifications/cancelled";
}

/// The reserved JSON-RPC 2.0 range. MCP adds no codes of its own; a
/// server is free to use anything outside −32768..=−32000, so never
/// match exhaustively on these.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

// ---------------------------------------------------------------- envelope

/// The `"jsonrpc": "2.0"` field as a type, not a `String`, so a message
/// we send can neither omit it nor carry the wrong version.
///
/// Reading is deliberately lax — any string is accepted and the field
/// may be absent. Rejecting a response we can otherwise understand over
/// a version marker that has had exactly one legal value since 2010
/// would trade a working tool call for pedantry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonRpcVersion;

impl Serialize for JsonRpcVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(|_| Self)
    }
}

/// A JSON-RPC id. The spec allows a string **or** a number, and real
/// servers echo back whichever shape they were given, so both have to
/// survive the round trip — a client that normalised numbers to strings
/// would fail to match its own replies.
///
/// `Hash + Eq` because M2 correlates pending requests through a
/// `HashMap<RequestId, _>`; that is why the numeric arm is `i64` and
/// not `f64`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        Self::Number(n)
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

/// A call that expects a reply. The presence of `id` is the only thing
/// separating this from a [`Notification`] on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: impl Into<RequestId>, method: &str, params: Option<Value>) -> Self {
        Self { jsonrpc: JsonRpcVersion, id: id.into(), method: method.to_string(), params }
    }
}

/// A one-way message. No `id`, therefore no reply — and a server that
/// receives a notification it does not recognise MUST stay silent, so
/// these fail invisibly when wrong. Hence [`method`]'s constants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(default)]
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: &str, params: Option<Value>) -> Self {
        Self { jsonrpc: JsonRpcVersion, method: method.to_string(), params }
    }
}

/// A JSON-RPC error object: the server refusing or failing to *run* the
/// method. Never the tool's own failure — [`CallToolResult::is_error`]
/// is that.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} (code {code})")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// What came back in a reply — **exactly one** of the two.
///
/// JSON-RPC says `result` and `error` are mutually exclusive, and the
/// distinction is load bearing for MCP specifically, because the two
/// failure channels mean genuinely different things:
///
/// - [`ResponsePayload::Error`] — the *protocol* failed. `-32602
///   Unknown tool`, a malformed request, a server that broke. The tool
///   never ran. Retrying the same call gets the same answer, and the
///   wording is aimed at us, not at the model.
/// - [`ResponsePayload::Result`] carrying a [`CallToolResult`] with
///   `isError: true` — the protocol worked perfectly and the *tool*
///   reported failure: a rate limit, a bad argument value, a 404 from
///   whatever it wraps. That text is for the model, which can read it
///   and try something else.
///
/// Collapsing them loses that: a typo'd tool name would be fed to the
/// model as if it were an API response, and a legitimate "file not
/// found" would look like a broken server. An enum rather than two
/// `Option` fields so neither "both" nor "neither" is representable —
/// [`Response`]'s `TryFrom` rejects those at the parse boundary, which
/// is the only place they can arise.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload {
    Result(Value),
    Error(RpcError),
}

/// A reply to one of our requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "WireResponse", into = "WireResponse")]
pub struct Response {
    /// `None` only when the server could not parse our request far
    /// enough to echo an id back (JSON-RPC mandates `null` there).
    /// Uncorrelatable by construction, so M2 can only log it — which is
    /// still better than dropping the message as garbage, because it is
    /// the one error that says *we* sent something broken.
    pub id: Option<RequestId>,
    pub payload: ResponsePayload,
}

impl Response {
    /// Borrowing view for callers that only want to branch.
    pub fn as_result(&self) -> Result<&Value, &RpcError> {
        match &self.payload {
            ResponsePayload::Result(v) => Ok(v),
            ResponsePayload::Error(e) => Err(e),
        }
    }

    pub fn into_result(self) -> Result<Value, RpcError> {
        match self.payload {
            ResponsePayload::Result(v) => Ok(v),
            ResponsePayload::Error(e) => Err(e),
        }
    }
}

/// The on-the-wire shape, where the exclusivity is unenforceable.
/// A separate struct rather than `#[serde(flatten)]` over an externally
/// tagged enum: flatten would swallow any *other* top-level field a
/// server invents into the enum and then fail on it, and the error for
/// a response carrying both keys would be unreadable.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireResponse {
    #[serde(default)]
    jsonrpc: JsonRpcVersion,
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

impl TryFrom<WireResponse> for Response {
    type Error = &'static str;

    fn try_from(w: WireResponse) -> Result<Self, &'static str> {
        let payload = match (w.result, w.error) {
            (Some(_), Some(_)) => return Err("JSON-RPC response has both `result` and `error`"),
            (None, None) => return Err("JSON-RPC response has neither `result` nor `error`"),
            (Some(r), None) => ResponsePayload::Result(r),
            (None, Some(e)) => ResponsePayload::Error(e),
        };
        Ok(Self { id: w.id, payload })
    }
}

impl From<Response> for WireResponse {
    fn from(r: Response) -> Self {
        let (result, error) = match r.payload {
            ResponsePayload::Result(v) => (Some(v), None),
            ResponsePayload::Error(e) => (None, Some(e)),
        };
        Self { jsonrpc: JsonRpcVersion, id: r.id, result, error }
    }
}

/// Anything a server may push down the same pipe.
///
/// A stdio server multiplexes replies, notifications and its own
/// requests onto one stdout, so the reader task cannot assume every
/// line answers something we sent. The `Request` arm exists even though
/// we implement no server-initiated method: a server asking for
/// `sampling/createMessage` blocks until it is answered, so M2 has to
/// recognise the shape in order to reply `-32601` instead of leaving
/// the server waiting forever.
///
/// Order matters — `untagged` takes the first arm that parses, and only
/// a `Response` carries `result`/`error`, only a `Request` carries both
/// `id` and `method`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Request(Request),
    Notification(Notification),
}

// ------------------------------------------------------------- initialize

/// `initialize` params. We advertise no client capabilities: nanopi
/// implements neither `roots`, `sampling` nor `elicitation`, and
/// claiming one we do not serve would hang any server that used it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

impl InitializeParams {
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ClientCapabilities {},
            client_info: Implementation {
                name: name.to_string(),
                title: None,
                version: version.to_string(),
            },
        }
    }
}

/// Empty, but serialised as `{}` rather than omitted — `capabilities`
/// is required by the spec, and some servers reject its absence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientCapabilities {}

/// Name/version of either end. `title` is the display name, added in
/// this revision; older servers omit it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    pub server_info: Implementation,
    /// Free text the server wants in the system prompt. Read but not
    /// used yet; M6 decides whether it earns the context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// What the server offers.
///
/// Only `tools` is typed, because tools are all this client uses.
/// Everything else — today's `prompts`, `resources`, `logging`,
/// `completions`, `experimental`, and whatever the next revision adds —
/// is kept verbatim in `other` instead of being dropped: `/mcp` (M7)
/// can then say what a server supports that we do not, which is the
/// difference between "this server is useless" and "this server also
/// has prompts we ignore".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// The server promises `notifications/tools/list_changed`. Absent
    /// means false, which is the safe reading: we re-list on our own
    /// schedule instead of trusting a notification that never comes.
    #[serde(default)]
    pub list_changed: bool,
}

// ------------------------------------------------------------ tools/list

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListToolsParams {
    /// Opaque; echo back exactly what `nextCursor` gave. Omitted on the
    /// first page — an explicit `null` is not the same thing to every
    /// server implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// `Some` means there is another page. Pagination is not optional
    /// to implement: a server with more tools than fit one page would
    /// otherwise register a silently truncated set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Kept as raw JSON Schema and passed through to the provider
    /// untouched. Modelling it would mean re-implementing a schema
    /// dialect we never evaluate ourselves — the model is the only
    /// consumer, and anything we failed to model would be silently
    /// dropped from the tool's contract.
    ///
    /// Defaulted rather than required, despite the spec requiring it:
    /// one server omitting it must not void an entire `tools/list`.
    #[serde(default = "empty_object_schema")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

fn empty_object_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

/// Behavioural hints. **Not a security boundary** — the spec says
/// clients MUST treat these as untrusted unless the server is trusted,
/// so `readOnlyHint` cannot be used to decide which tools an
/// orchestrator may call itself (see the rejected variant in
/// `docs/mcp.md`). Display and prompting only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

// ------------------------------------------------------------ tools/call

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    /// Always serialised, even when empty. Servers built on the older
    /// SDKs treat a missing `arguments` as a malformed call, and `{}`
    /// is valid against every `inputSchema` that requires nothing.
    #[serde(default)]
    pub arguments: Map<String, Value>,
}

impl CallToolParams {
    pub fn new(name: &str, arguments: Map<String, Value>) -> Self {
        Self { name: name.to_string(), arguments }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    /// Defaulted: a server returning only `structuredContent` is out of
    /// spec but harmless, and M5 can still render the structured half.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    /// Server-produced data matching the tool's `outputSchema`. Maps to
    /// `ToolOutput::metadata`. Unrelated to LLM "structured outputs".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// The **tool** failed, not the protocol — see [`ResponsePayload`].
    /// Absent means false, so a server that never sets it is reporting
    /// success, which is the reading the spec's own examples assume.
    #[serde(default)]
    pub is_error: bool,
}

/// One item of a tool's unstructured output.
///
/// The catch-all arm is the point of this enum. Content types are added
/// between spec revisions, and a server is free to send one the day it
/// is published; without [`ContentBlock::Unknown`] a single new block
/// type would fail `serde_json::from_value` for the *whole*
/// `CallToolResult` and turn a successful tool call into a protocol
/// error. Keeping the raw JSON means M5 can degrade it to a text
/// summary — something the model can read beats nothing it can.
///
/// `untagged` on the last arm also absorbs malformed known blocks (an
/// `image` with no `data`, say), which is the same trade for the same
/// reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
    #[serde(rename_all = "camelCase")]
    Image {
        /// base64, no data-URI prefix.
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
    #[serde(rename_all = "camelCase")]
    Audio {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
    /// A pointer to a resource the client may fetch. nanopi does not
    /// implement `resources/*`, so M5 degrades these to a text summary
    /// rather than dereferencing them.
    #[serde(rename_all = "camelCase")]
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
    Resource {
        resource: ResourceContents,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Annotations>,
    },
    #[serde(untagged)]
    Unknown(Value),
}

impl ContentBlock {
    /// The wire `type`, including for [`ContentBlock::Unknown`], where
    /// it is whatever string the server sent. M5 needs it to label the
    /// degraded summary: "[unsupported content: video]" tells the model
    /// something; "[unsupported content]" does not.
    pub fn kind(&self) -> &str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
            Self::ResourceLink { .. } => "resource_link",
            Self::Resource { .. } => "resource",
            Self::Unknown(v) => v.get("type").and_then(Value::as_str).unwrap_or("unknown"),
        }
    }
}

/// An embedded resource's body. The spec splits this into text and blob
/// variants; one struct with both optional instead, because a server
/// sending neither (or both) is a malformed body we can still summarise
/// — an enum would reject the surrounding tool result outright.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceContents {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// Optional metadata on any content block: who it is for and how much
/// it matters. `audience` is `Vec<String>` rather than a `user`/
/// `assistant` enum — an unrecognised role is not worth failing a tool
/// result over, and nothing here is acted on, only displayed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotations {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audience: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every payload below is copied from
    /// <https://modelcontextprotocol.io/specification/2025-06-18/server/tools>
    /// and <../basic/lifecycle>, byte for byte. Hand-written fixtures
    /// would only ever test our own assumptions back at us.
    fn parse_result<T: serde::de::DeserializeOwned>(raw: &str) -> T {
        let response: Response = serde_json::from_str(raw).expect("envelope must parse");
        let value = response.as_result().expect("spec payload is a success response").clone();
        serde_json::from_value(value).expect("result payload must parse")
    }

    #[test]
    fn initialize_result_from_the_spec() {
        let got: InitializeResult = parse_result(
            r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {
                  "logging": {},
                  "prompts": { "listChanged": true },
                  "resources": { "subscribe": true, "listChanged": true },
                  "tools": { "listChanged": true }
                },
                "serverInfo": {
                  "name": "ExampleServer",
                  "title": "Example Server Display Name",
                  "version": "1.0.0"
                },
                "instructions": "Optional instructions for the client"
              }
            }"#,
        );

        assert_eq!(got.protocol_version, PROTOCOL_VERSION);
        assert_eq!(got.server_info.name, "ExampleServer");
        assert_eq!(got.server_info.title.as_deref(), Some("Example Server Display Name"));
        assert!(got.capabilities.tools.expect("tools capability").list_changed);
        assert_eq!(got.instructions.as_deref(), Some("Optional instructions for the client"));
        // Capabilities we do not model are kept, not dropped — /mcp
        // reports them so a server does not look emptier than it is.
        let mut unmodelled: Vec<&String> = got.capabilities.other.keys().collect();
        unmodelled.sort();
        assert_eq!(unmodelled, ["logging", "prompts", "resources"]);
    }

    /// A capability key no revision has invented yet must not stop a
    /// server from connecting.
    #[test]
    fn an_unknown_capability_does_not_fail_initialize() {
        let got: ServerCapabilities =
            serde_json::from_str(r#"{ "tools": {}, "telepathy": { "range": "far" } }"#)
                .expect("unknown capability keys must parse");
        assert!(!got.tools.expect("tools").list_changed, "absent listChanged reads as false");
        assert!(got.other.contains_key("telepathy"));
    }

    #[test]
    fn tools_list_result_from_the_spec() {
        let got: ListToolsResult = parse_result(
            r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "result": {
                "tools": [
                  {
                    "name": "get_weather",
                    "title": "Weather Information Provider",
                    "description": "Get current weather information for a location",
                    "inputSchema": {
                      "type": "object",
                      "properties": {
                        "location": {
                          "type": "string",
                          "description": "City name or zip code"
                        }
                      },
                      "required": ["location"]
                    }
                  }
                ],
                "nextCursor": "next-page-cursor"
              }
            }"#,
        );

        assert_eq!(got.next_cursor.as_deref(), Some("next-page-cursor"), "pagination must survive");
        assert_eq!(got.tools.len(), 1);
        let tool = &got.tools[0];
        assert_eq!(tool.name, "get_weather");
        assert_eq!(tool.title.as_deref(), Some("Weather Information Provider"));
        // The schema is passed to the provider verbatim, so the nested
        // shape has to come through untouched, not flattened.
        assert_eq!(tool.input_schema["properties"]["location"]["type"], "string");
        assert_eq!(tool.input_schema["required"][0], "location");
        assert!(tool.output_schema.is_none());
    }

    #[test]
    fn tool_with_an_output_schema_from_the_spec() {
        let tool: Tool = serde_json::from_str(
            r#"{
              "name": "get_weather_data",
              "title": "Weather Data Retriever",
              "description": "Get current weather data for a location",
              "inputSchema": {
                "type": "object",
                "properties": { "location": { "type": "string" } },
                "required": ["location"]
              },
              "outputSchema": {
                "type": "object",
                "properties": {
                  "temperature": { "type": "number" },
                  "conditions": { "type": "string" },
                  "humidity": { "type": "number" }
                },
                "required": ["temperature", "conditions", "humidity"]
              }
            }"#,
        )
        .expect("spec tool must parse");
        assert_eq!(tool.output_schema.expect("outputSchema")["required"][0], "temperature");
    }

    /// Out of spec, but one such server must not void the whole list.
    #[test]
    fn a_tool_without_an_input_schema_still_registers() {
        let tool: Tool = serde_json::from_str(r#"{ "name": "ping" }"#).expect("must not fail");
        assert_eq!(tool.input_schema, serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn tools_call_result_with_mixed_content() {
        // The spec's text result, plus its image block — a server is
        // free to return both in one array and M5 splits them into
        // `content` and `images`.
        let got: CallToolResult = parse_result(
            r#"{
              "jsonrpc": "2.0",
              "id": 2,
              "result": {
                "content": [
                  {
                    "type": "text",
                    "text": "Current weather in New York:\nTemperature: 72°F\nConditions: Partly cloudy"
                  },
                  {
                    "type": "image",
                    "data": "base64-encoded-data",
                    "mimeType": "image/png",
                    "annotations": {
                      "audience": ["user"],
                      "priority": 0.9
                    }
                  }
                ],
                "isError": false
              }
            }"#,
        );

        assert!(!got.is_error);
        assert_eq!(got.content.len(), 2);
        match &got.content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(text.starts_with("Current weather in New York:\nTemperature"))
            }
            other => panic!("expected text, got {other:?}"),
        }
        match &got.content[1] {
            ContentBlock::Image { data, mime_type, annotations } => {
                assert_eq!(data, "base64-encoded-data");
                assert_eq!(mime_type, "image/png");
                let a = annotations.as_ref().expect("annotations");
                assert_eq!(a.audience, ["user"]);
                assert_eq!(a.priority, Some(0.9));
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn audio_resource_link_and_embedded_resource_from_the_spec() {
        let blocks: Vec<ContentBlock> = serde_json::from_str(
            r#"[
              { "type": "audio", "data": "base64-encoded-audio-data", "mimeType": "audio/wav" },
              {
                "type": "resource_link",
                "uri": "file:///project/src/main.rs",
                "name": "main.rs",
                "description": "Primary application entry point",
                "mimeType": "text/x-rust",
                "annotations": { "audience": ["assistant"], "priority": 0.9 }
              },
              {
                "type": "resource",
                "resource": {
                  "uri": "file:///project/src/main.rs",
                  "mimeType": "text/x-rust",
                  "text": "fn main() {\n    println!(\"Hello world!\");\n}"
                },
                "annotations": {
                  "audience": ["user", "assistant"],
                  "priority": 0.7,
                  "lastModified": "2025-05-03T14:30:00Z"
                }
              }
            ]"#,
        )
        .expect("spec content blocks must parse");

        assert_eq!(
            blocks.iter().map(ContentBlock::kind).collect::<Vec<_>>(),
            ["audio", "resource_link", "resource"],
            "a block that fell through to Unknown would show up here"
        );
        match &blocks[2] {
            ContentBlock::Resource { resource, annotations } => {
                assert!(resource.text.as_deref().unwrap_or_default().contains("Hello world!"));
                assert!(resource.blob.is_none());
                let a = annotations.as_ref().expect("annotations");
                assert_eq!(a.last_modified.as_deref(), Some("2025-05-03T14:30:00Z"));
            }
            other => panic!("expected embedded resource, got {other:?}"),
        }
    }

    #[test]
    fn structured_content_result_from_the_spec() {
        let got: CallToolResult = parse_result(
            r#"{
              "jsonrpc": "2.0",
              "id": 5,
              "result": {
                "content": [
                  { "type": "text", "text": "{\"temperature\": 22.5, \"conditions\": \"Partly cloudy\", \"humidity\": 65}" }
                ],
                "structuredContent": {
                  "temperature": 22.5,
                  "conditions": "Partly cloudy",
                  "humidity": 65
                }
              }
            }"#,
        );
        assert_eq!(got.structured_content.expect("structuredContent")["humidity"], 65);
        assert!(!got.is_error, "an absent isError means the tool succeeded");
    }

    /// The tool ran and reported failure. This is a *successful*
    /// response — the text is for the model to read and act on.
    #[test]
    fn tool_execution_error_from_the_spec() {
        let raw = r#"{
          "jsonrpc": "2.0",
          "id": 4,
          "result": {
            "content": [
              { "type": "text", "text": "Failed to fetch weather data: API rate limit exceeded" }
            ],
            "isError": true
          }
        }"#;
        let response: Response = serde_json::from_str(raw).unwrap();
        let value = response.as_result().expect("isError lives INSIDE a success response").clone();
        let got: CallToolResult = serde_json::from_value(value).unwrap();
        assert!(got.is_error);
        assert_eq!(got.content.len(), 1, "the failure text must reach the model");
    }

    /// The tool never ran. Same `tools/call`, completely different
    /// meaning — conflating the two would feed a typo'd tool name to
    /// the model as if it were an API response.
    #[test]
    fn protocol_error_from_the_spec() {
        let response: Response = serde_json::from_str(
            r#"{
              "jsonrpc": "2.0",
              "id": 3,
              "error": { "code": -32602, "message": "Unknown tool: invalid_tool_name" }
            }"#,
        )
        .expect("an error response must parse");

        assert_eq!(response.id, Some(RequestId::Number(3)), "errors still correlate");
        let err = response.as_result().expect_err("must not look like a result");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert_eq!(err.to_string(), "Unknown tool: invalid_tool_name (code -32602)");
    }

    #[test]
    fn protocol_error_carrying_data_from_the_spec() {
        let response: Response = serde_json::from_str(
            r#"{
              "jsonrpc": "2.0",
              "id": 1,
              "error": {
                "code": -32602,
                "message": "Unsupported protocol version",
                "data": { "supported": ["2024-11-05"], "requested": "1.0.0" }
              }
            }"#,
        )
        .unwrap();
        let err = response.as_result().expect_err("error");
        assert_eq!(err.data.as_ref().expect("data")["supported"][0], "2024-11-05");
    }

    /// The headline requirement: the spec adds content types, and a
    /// server may send one the day it ships. It must cost us that one
    /// block, not the tool call.
    ///
    /// Teeth: delete the `#[serde(untagged)] Unknown` arm and this
    /// fails at the `expect`, taking the sibling text block with it.
    #[test]
    fn an_unknown_content_type_does_not_fail_the_result() {
        let got: CallToolResult = serde_json::from_str(
            r#"{
              "content": [
                { "type": "text", "text": "here is a clip" },
                { "type": "video", "data": "base64", "mimeType": "video/mp4" }
              ]
            }"#,
        )
        .expect("an unrecognised block must not fail the whole response");

        assert_eq!(got.content.len(), 2, "the unknown block is kept, not silently dropped");
        assert!(matches!(got.content[0], ContentBlock::Text { .. }), "the sibling survives");
        match &got.content[1] {
            ContentBlock::Unknown(v) => assert_eq!(v["mimeType"], "video/mp4"),
            other => panic!("expected the catch-all, got {other:?}"),
        }
        // Enough to label the degraded summary M5 will render.
        assert_eq!(got.content[1].kind(), "video");
    }

    /// Same trade, applied to a known type the server got wrong: an
    /// `image` with no `data` degrades instead of failing the call.
    #[test]
    fn a_malformed_known_block_degrades_rather_than_failing() {
        let blocks: Vec<ContentBlock> =
            serde_json::from_str(r#"[{ "type": "image", "mimeType": "image/png" }]"#)
                .expect("must not fail");
        assert!(matches!(blocks[0], ContentBlock::Unknown(_)));
        assert_eq!(blocks[0].kind(), "image");
    }

    // ------------------------------------------------------- round trips

    /// The requests we actually send, through serialize → deserialize.
    #[test]
    fn requests_we_send_round_trip() {
        let initialize = Request::new(
            1,
            method::INITIALIZE,
            Some(serde_json::to_value(InitializeParams::new("nanopi", "0.14.0")).unwrap()),
        );
        let list = Request::new(
            2,
            method::TOOLS_LIST,
            Some(serde_json::to_value(ListToolsParams { cursor: Some("c1".into()) }).unwrap()),
        );
        let mut args = Map::new();
        args.insert("location".into(), Value::String("New York".into()));
        let call = Request::new(
            "call-3",
            method::TOOLS_CALL,
            Some(serde_json::to_value(CallToolParams::new("get_weather", args)).unwrap()),
        );

        for req in [initialize, list, call] {
            let text = serde_json::to_string(&req).unwrap();
            let back: Request = serde_json::from_str(&text).unwrap();
            assert_eq!(back, req, "round trip changed the request: {text}");
            assert!(text.contains(r#""jsonrpc":"2.0""#), "missing version: {text}");
        }
    }

    /// A string id and a numeric id are different ids, and a server
    /// echoes back whichever it was given. Normalising either way would
    /// break correlation in M2.
    #[test]
    fn request_ids_keep_their_json_type() {
        for (raw, expected) in [
            ("1", RequestId::Number(1)),
            (r#""1""#, RequestId::String("1".into())),
            (r#""call-3""#, RequestId::String("call-3".into())),
        ] {
            let id: RequestId = serde_json::from_str(raw).unwrap();
            assert_eq!(id, expected);
            assert_eq!(serde_json::to_string(&id).unwrap(), raw, "must re-serialise unchanged");
        }
    }

    /// The handshake's second half. `params` and `id` must both be
    /// absent: an `id` would turn it into a request the server must
    /// answer, and it has no answer to give.
    #[test]
    fn the_initialized_notification_is_exactly_the_spec_payload() {
        let text = serde_json::to_string(&Notification::new(method::INITIALIZED, None)).unwrap();
        assert_eq!(text, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    #[test]
    fn a_tools_call_with_no_arguments_still_sends_an_object() {
        let text = serde_json::to_string(&CallToolParams::new("ping", Map::new())).unwrap();
        assert_eq!(text, r#"{"name":"ping","arguments":{}}"#, "omitting it breaks some servers");
    }

    #[test]
    fn responses_round_trip_through_both_arms() {
        for payload in [
            ResponsePayload::Result(serde_json::json!({ "tools": [] })),
            ResponsePayload::Error(RpcError {
                code: error_code::METHOD_NOT_FOUND,
                message: "nope".into(),
                data: None,
            }),
        ] {
            let response = Response { id: Some(RequestId::Number(7)), payload };
            let text = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&text).unwrap(), response);
        }
    }

    /// `result` and `error` are mutually exclusive, and the only place
    /// that can be violated is the parse boundary — which is why
    /// `ResponsePayload` is an enum and not two `Option` fields.
    #[test]
    fn a_response_must_carry_exactly_one_of_result_and_error() {
        let both = r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#;
        let neither = r#"{"jsonrpc":"2.0","id":1}"#;
        for raw in [both, neither] {
            assert!(serde_json::from_str::<Response>(raw).is_err(), "must reject: {raw}");
        }
    }

    /// JSON-RPC mandates a null id when the server could not read our
    /// request far enough to echo one. Uncorrelatable, but it is the
    /// one error that says we sent something broken, so M2 needs to see
    /// it rather than write it off as garbage on stdout.
    #[test]
    fn a_null_id_error_still_parses() {
        let response: Response = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#,
        )
        .expect("a null id must not make the message unreadable");
        assert!(response.id.is_none());
        assert_eq!(response.as_result().expect_err("error").code, error_code::PARSE_ERROR);
    }

    /// One stdout carries replies, notifications and the server's own
    /// requests. The reader task has to tell them apart — in particular
    /// a server *request* must not be mistaken for a notification, or
    /// it waits forever for a reply we never send.
    #[test]
    fn incoming_messages_are_demultiplexed_by_shape() {
        let cases = [
            (r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, "response"),
            (r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}"#, "response"),
            (r#"{"jsonrpc":"2.0","id":9,"method":"sampling/createMessage"}"#, "request"),
            (r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#, "notification"),
        ];
        for (raw, expected) in cases {
            let got = match serde_json::from_str::<Incoming>(raw).expect(raw) {
                Incoming::Response(_) => "response",
                Incoming::Request(_) => "request",
                Incoming::Notification(_) => "notification",
            };
            assert_eq!(got, expected, "misrouted: {raw}");
        }
    }
}
