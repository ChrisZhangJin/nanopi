//! nanopi v0.1 — minimal demo
//!
//! Connects to an OpenAI-compatible endpoint, streams the response to stdout,
//! persists the conversation as one JSONL line per entry under
//! `~/.nanopi/sessions/<id>.jsonl`.
//!
//! Usage:
//!   nanopi --api-key sk-xxx --model gpt-4o-mini --message "你好"
//!   nanopi --base-url http://localhost:11434/v1 --model llama3 --message "hi"
//!
//! What this v0.1 DOES:
//!   - CLI parsing (clap)
//!   - HTTP POST to OpenAI-compatible /chat/completions
//!   - Hand-written SSE parsing of the streaming response
//!   - Token-by-token stdout output
//!   - JSONL session append
//!
//! What this v0.1 DOES NOT (yet):
//!   - No TUI (raw stdin/stdout)
//!   - No tool calls
//!   - No Anthropic provider
//!   - No skills/prompts/trust
//!   - No sessions listing/resuming

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// ---------- CLI ----------

#[derive(Parser, Debug)]
#[command(name = "nanopi", version, about = "minimal Pi port in Rust (v0.1 demo)")]
struct Args {
    /// OpenAI-compatible API base URL.
    /// Examples:
    ///   https://api.openai.com/v1
    ///   https://api.deepseek.com/v1
    ///   http://localhost:11434/v1   (ollama)
    #[arg(long, default_value = "https://api.openai.com/v1")]
    base_url: String,

    /// Model identifier (provider-specific).
    #[arg(long)]
    model: String,

    /// API key. Falls back to OPENAI_API_KEY env var if omitted.
    #[arg(long)]
    api_key: Option<String>,

    /// User message. If omitted, read from stdin.
    #[arg(long)]
    message: Option<String>,
}

// ---------- OpenAI-compatible wire types ----------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<RequestMessage<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// One SSE-decoded chunk from the streaming response.
#[derive(Deserialize, Debug)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
}

#[derive(Deserialize, Debug)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

// ---------- JSONL session entries ----------

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum SessionEntry {
    #[serde(rename = "session")]
    Header {
        version: u32,
        id: String,
        timestamp: String,
        model: String,
        base_url: String,
    },
    #[serde(rename = "message")]
    Message {
        id: String,
        timestamp: String,
        role: String,
        content: String,
    },
}

// ---------- main ----------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Resolve API key: flag > env var.
    let api_key = match args.api_key {
        Some(k) => k,
        None => std::env::var("OPENAI_API_KEY")
            .context("no --api-key and OPENAI_API_KEY env var is unset")?,
    };

    // Resolve user message: flag > stdin.
    let user_message = match args.message {
        Some(m) => m,
        None => {
            eprintln!("(reading message from stdin; Ctrl-D to finish)");
            let mut buf = String::new();
            io::stdin().read_line(&mut buf)?;
            if buf.is_empty() {
                bail!("no message provided via --message or stdin");
            }
            buf.trim_end().to_string()
        }
    };

    // Build HTTP client.
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build HTTP client")?;

    // Build request payload.
    let req = ChatRequest {
        model: &args.model,
        messages: vec![RequestMessage {
            role: "user",
            content: &user_message,
        }],
        stream: true,
    };

    // Fire request.
    eprintln!("→ POST {}/chat/completions (model={})", args.base_url, args.model);
    let resp = client
        .post(format!("{}/chat/completions", args.base_url.trim_end_matches('/')))
        .bearer_auth(&api_key)
        .json(&req)
        .send()
        .await
        .context("HTTP request failed")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {}: {}", status, body);
    }

    // Set up session file.
    let session_path = new_session_path()?;
    append_session_entry(
        &session_path,
        &SessionEntry::Header {
            version: 1,
            id: uuid_like(),
            timestamp: now_iso8601(),
            model: args.model.clone(),
            base_url: args.base_url.clone(),
        },
    )?;
    append_session_entry(
        &session_path,
        &SessionEntry::Message {
            id: uuid_like(),
            timestamp: now_iso8601(),
            role: "user".into(),
            content: user_message.clone(),
        },
    )?;

    // Set up streaming parser → stdout + session file.
    let (tx, mut rx) = mpsc::channel::<String>(64);

    // Spawn SSE reader task. We hand-split on newlines because reqwest's
    // bytes_stream() returns a `Stream<Bytes>` (not AsyncRead), so we can't
    // directly BufReader it. Build a tiny line buffer.
    let stream_task = {
        tokio::spawn(async move {
            use futures_util::stream::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk_res) = stream.next().await {
                let chunk = match chunk_res {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("\n[warn] HTTP stream error: {e}");
                        break;
                    }
                };
                buf.extend_from_slice(&chunk);
                // Drain complete lines (each ends with \n; SSE servers send \n\n between events).
                while let Some(idx) = buf.iter().position(|&b| b == b'\n') {
                    let line_bytes: Vec<u8> = buf.drain(..=idx).collect();
                    let line = match std::str::from_utf8(&line_bytes[..line_bytes.len() - 1]) {
                        Ok(s) => s.trim_end(),
                        Err(_) => continue, // skip non-UTF8 (e.g. keep-alive comments)
                    };
                    let Some(payload) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    if payload == "[DONE]" {
                        return;
                    }
                    match serde_json::from_str::<StreamChunk>(payload) {
                        Ok(chunk) => {
                            for choice in chunk.choices {
                                if let Some(text) = choice.delta.content {
                                    if tx.send(text).await.is_err() {
                                        return; // receiver dropped
                                    }
                                }
                                if choice.finish_reason.is_some() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("\n[warn] bad SSE chunk: {e}; payload={payload}");
                        }
                    }
                }
            }
        })
    };

    // Render to stdout, accumulate, then save.
    let mut accumulated = String::new();
    let mut stdout = io::stdout().lock();
    print!("\x1b[1;32m"); // green foreground for assistant text
    while let Some(token) = rx.recv().await {
        write!(stdout, "{}", token)?;
        stdout.flush()?;
        accumulated.push_str(&token);
    }
    print!("\x1b[0m\n");
    drop(stdout);

    // Wait for the SSE task to finish.
    if let Err(e) = stream_task.await {
        eprintln!("[warn] SSE task join error: {e}");
    }

    // Persist the assistant turn.
    append_session_entry(
        &session_path,
        &SessionEntry::Message {
            id: uuid_like(),
            timestamp: now_iso8601(),
            role: "assistant".into(),
            content: accumulated,
        },
    )?;

    eprintln!("\n✓ session saved to {}", session_path.display());
    Ok(())
}

// ---------- helpers ----------

fn new_session_path() -> Result<PathBuf> {
    let base = dirs::home_dir().context("no home directory")?.join(".nanopi/sessions");
    std::fs::create_dir_all(&base).context("create ~/.nanopi/sessions")?;
    Ok(base.join(format!("{}.jsonl", uuid_like())))
}

fn append_session_entry(path: &PathBuf, entry: &SessionEntry) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let line = serde_json::to_string(entry)?;
    writeln!(f, "{}", line)?;
    f.flush()?;
    Ok(())
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Bare RFC3339-ish without tz: "YYYY-MM-DDTHH:MM:SSZ" computed from secs.
    // Good enough for v0.1 demo.
    format_iso8601_utc(secs)
}

fn format_iso8601_utc(secs: u64) -> String {
    // Days since 1970-01-01.
    let days = secs / 86400;
    let secs_of_day = secs % 86400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Civil-from-days algorithm (Howard Hinnant).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, minute, second
    )
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Not a real UUID, just a v0.1 demo identifier.
    format!("{:x}", nanos)
}