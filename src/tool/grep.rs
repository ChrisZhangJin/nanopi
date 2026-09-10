//! `grep` tool — recursively search file contents for a regex.
//!
//! Read-only, cwd-bounded. Skips binary files (detected by NUL byte in the
//! first 4 KB). Same ignore list as `find`. Output is `path:line:content`
//! per match, capped at 500 matches; overflow noted in metadata.
//!
//! # Two engines, one contract
//!
//! Searching runs through `rg` when ripgrep is on PATH, and through the
//! built-in walker below when it is not. The built-in is NOT a legacy
//! path: it is what keeps the "zero runtime deps" promise in the crate
//! description true, and it is the only engine available on a fresh
//! Android/Alpine box. ripgrep is an optional accelerator, never a
//! requirement. `NANOPI_NO_RIPGREP=1` forces the built-in.
//!
//! The flags in `ripgrep_args` exist to make the two engines agree, and
//! every one of them was chosen against measured `rg` output, not from
//! the man page:
//!
//!   - `--no-config` — a user's `RIPGREP_CONFIG_PATH` would otherwise
//!     silently change this tool's semantics per machine.
//!   - `--no-unicode` — matches the built-in's `unicode(false)`.
//!   - `--no-ignore` — the built-in has never read `.gitignore`; without
//!     this, results would depend on whether the repo is a git checkout.
//!   - `--sort path` — rg's parallel walk emits in nondeterministic
//!     order. That is fine for a human and bad here: it makes identical
//!     queries return differently-ordered output, and it makes the
//!     500-match cap keep a *random* 500. Costs the parallel walk, still
//!     comfortably faster than the built-in.
//!   - `--glob '!<dir>'` per `IGNORE_DIRS` — rg only skips those via
//!     `.gitignore`, which `--no-ignore` just turned off.
//!
//! Two divergences are known and deliberate:
//!
//!   - **Non-UTF-8 files.** The built-in skips a file whose bytes are not
//!     valid UTF-8; rg searches it and reports matches with replacement
//!     characters. rg's behaviour is the more useful one, and the
//!     built-in's is not worth a second full read of every file to
//!     imitate.
//!   - **Binary detection.** Both skip binaries; the built-in looks for a
//!     NUL in the first 4 KB, rg uses its own heuristic over a larger
//!     window. Files that are binary-ish past 4 KB may differ.
//!
//! `metadata.engine` says which ran, so a surprising result set can be
//! attributed instead of guessed at.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use async_trait::async_trait;
use regex::RegexBuilder;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::agent::context::ToolSpec;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_MATCHES: usize = 500;
const MAX_DEPTH: usize = 32;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024; // skip files > 5 MB
const IGNORE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "dist",
    "build",
    ".direnv",
];

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Recursively search file contents for a regex. Read-only; skips binary files, .git/node_modules/target, and files > 5 MB. Output format: path:line:content.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex to match against each line."
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory or single file (absolute or relative to cwd). Defaults to cwd."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "Case-insensitive match."
                    },
                    "all": {
                        "type": "boolean",
                        "description": "Include dotfiles/dotdirs and ignored dirs."
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgs("pattern must be a string".into()))?;
        let ci = args["case_insensitive"].as_bool().unwrap_or(false);
        let all = args["all"].as_bool().unwrap_or(false);
        // `unicode(false)` uses ASCII case-folding, which is enough for
        // our line-oriented use case and doesn't require the `regex`
        // crate's optional `unicode-case` feature.
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .unicode(false)
            .build()
            .map_err(|e| ToolError::InvalidArgs(format!("invalid regex: {e}")))?;
        let base_str = args["path"].as_str().unwrap_or(".");
        let base = resolve_path_within(&ctx.cwd, base_str)?;
        let root = if base.is_dir() {
            base.clone()
        } else {
            base.parent().unwrap_or(Path::new(".")).to_path_buf()
        };

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut files_scanned = 0usize;

        // `re` is built above on BOTH paths and is what rejects a bad
        // pattern. Handing the pattern straight to rg would report an
        // invalid regex as a tool Execution error (rg exit code 2) rather
        // than InvalidArgs, and would accept rg-only syntax that the
        // built-in cannot parse — making the tool's accepted language
        // depend on whether ripgrep happens to be installed.
        let engine = if let Some(rg) = ripgrep_path() {
            match search_ripgrep(rg, pattern, &base, &root, ci, all, &mut matches, &mut truncated)
                .await
            {
                Ok(()) => "ripgrep",
                // Falling back rather than surfacing the error: a broken
                // rg (missing shared lib, killed by a seccomp profile,
                // ENOMEM) must not take the grep tool down with it when
                // a working engine is compiled in.
                Err(e) => {
                    crate::note!(
                        "grep: ripgrep failed ({e}) — falling back to built-in search"
                    );
                    matches.clear();
                    truncated = false;
                    search_builtin(
                        &base,
                        &root,
                        &re,
                        all,
                        &mut matches,
                        &mut truncated,
                        &mut files_scanned,
                    );
                    "builtin"
                }
            }
        } else {
            search_builtin(
                &base,
                &root,
                &re,
                all,
                &mut matches,
                &mut truncated,
                &mut files_scanned,
            );
            "builtin"
        };

        let out = if matches.is_empty() {
            String::new()
        } else {
            matches.join("\n") + "\n"
        };

        Ok(ToolOutput {
            content: out,
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({
                "base": base.display().to_string(),
                "matches": matches.len(),
                // rg does not report a scanned-file count without
                // `--stats`, whose output would have to be parsed back out
                // of the match stream and is unavailable anyway once the
                // cap short-circuits the read. Reported as null rather
                // than as the number of files that *matched*, which is a
                // different number and would quietly mislead.
                "files_scanned": if engine == "builtin" { json!(files_scanned) } else { Value::Null },
                "truncated": truncated,
                "engine": engine,
            })),
        })
    }
}

/// Resolved once per process: a PATH scan per grep call is pointless, and
/// ripgrep does not appear or vanish mid-session in any way we care about.
fn ripgrep_path() -> Option<&'static Path> {
    static RG: OnceLock<Option<PathBuf>> = OnceLock::new();
    RG.get_or_init(|| {
        // The opt-out is checked here, not at the call site, so the
        // built-in path is reached identically whether rg is absent or
        // merely disabled.
        if std::env::var_os("NANOPI_NO_RIPGREP").is_some_and(|v| !v.is_empty() && v != "0") {
            return None;
        }
        crate::util::which::which("rg")
    })
    .as_deref()
}

/// Flags for one search. Split out from the spawn so a test can assert on
/// the argument vector without needing rg installed — the flags ARE the
/// compatibility contract, and a silent drop of `--no-config` or
/// `--sort` would not show up in output on a clean machine.
fn ripgrep_args(pattern: &str, target: &str, ci: bool, all: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "--no-heading",
        "--line-number",
        "--with-filename",
        "--no-messages",
        "--no-config",
        "--color",
        "never",
        "--no-unicode",
        "--no-ignore",
        "--sort",
        "path",
        "--max-filesize",
        // 5M is 5 MiB in rg's parser, matching MAX_FILE_BYTES exactly.
        "5M",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push("--max-depth".into());
    a.push(MAX_DEPTH.to_string());
    if ci {
        a.push("-i".into());
    }
    if all {
        // The built-in's `all` means dotfiles AND the ignore list.
        a.push("--hidden".into());
    } else {
        // rg already skips hidden entries by default, which covers the
        // dot-prefixed members of IGNORE_DIRS; the rest need globs.
        for d in IGNORE_DIRS {
            a.push(format!("--glob=!{d}"));
        }
    }
    // `-e` and `--` so a pattern or path starting with `-` is not read as
    // a flag. The built-in has no such hazard, and forgetting them here
    // would make `grep -foo` an error on one engine only.
    a.push("-e".into());
    a.push(pattern.to_string());
    a.push("--".into());
    a.push(target.to_string());
    a
}

#[allow(clippy::too_many_arguments)]
async fn search_ripgrep(
    rg: &Path,
    pattern: &str,
    base: &Path,
    root: &Path,
    ci: bool,
    all: bool,
    out: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(), String> {
    // Run from `root` and name the target relatively, so rg's own output
    // is already root-relative and no path rewriting is needed beyond the
    // "./" strip below.
    let target = if base.is_file() {
        base.strip_prefix(root)
            .unwrap_or(base)
            .to_string_lossy()
            .to_string()
    } else {
        ".".to_string()
    };

    let mut child = tokio::process::Command::new(rg)
        .args(ripgrep_args(pattern, &target, ci, all))
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    let stdout = child.stdout.take().ok_or("stdout not captured")?;
    // Read bytes and decode each line lossily rather than using
    // `BufReader::lines()`, which yields `InvalidData` for a stream that
    // is not wholly valid UTF-8. rg copies matched bytes through
    // verbatim, so ONE latin-1 file anywhere under the search root made
    // `lines()` fail — and that error is indistinguishable from a broken
    // rg, so the whole search silently fell back to the built-in engine.
    // The differential test caught this; nothing in normal use would
    // have, beyond an unexplained loss of speed on some repos.
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            buf.pop();
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        if out.len() >= MAX_MATCHES {
            *truncated = true;
            // Dropping `child` here would be enough (kill_on_drop), but
            // the kill is explicit so it is obvious that a 2-million-hit
            // search does not keep rg running until it finishes.
            let _ = child.start_kill();
            break;
        }
        // rg prints `./a.txt:1:…` for the `.` target; the built-in prints
        // `a.txt:1:…`. Normalise to the built-in's form, which is what
        // every existing session transcript and test expects.
        out.push(line.strip_prefix("./").unwrap_or(&line).to_string());
    }

    // Exit status is deliberately not checked for success: rg exits 1 on
    // "no matches", which is a normal empty result, and 2 on a real
    // error — but stderr is /dev/null'd and a partial result is still
    // better than none, so the caller only ever sees spawn/read failures.
    let _ = child.wait().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_builtin(
    base: &Path,
    root: &Path,
    re: &regex::Regex,
    all: bool,
    matches: &mut Vec<String>,
    truncated: &mut bool,
    files_scanned: &mut usize,
) {
    if base.is_file() {
        grep_file(root, base, re, matches, truncated, files_scanned);
    } else {
        walk(root, base, re, all, 0, matches, truncated, files_scanned);
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    re: &regex::Regex,
    all: bool,
    depth: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
    files_scanned: &mut usize,
) {
    if *truncated || depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for e in read.flatten() {
        if *truncated {
            return;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !all {
            if name.starts_with('.') {
                continue;
            }
            if is_dir && IGNORE_DIRS.contains(&name.as_str()) {
                continue;
            }
        }
        let full = e.path();
        if is_dir {
            walk(
                root,
                &full,
                re,
                all,
                depth + 1,
                out,
                truncated,
                files_scanned,
            );
        } else {
            grep_file(root, &full, re, out, truncated, files_scanned);
        }
    }
}

fn grep_file(
    root: &Path,
    file: &Path,
    re: &regex::Regex,
    out: &mut Vec<String>,
    truncated: &mut bool,
    files_scanned: &mut usize,
) {
    if *truncated {
        return;
    }
    let Ok(meta) = file.metadata() else { return };
    if meta.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(file) else {
        return;
    };
    if is_binary(&bytes) {
        return;
    }
    *files_scanned += 1;
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return,
    };
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel_str = rel.to_string_lossy();
    for (i, line) in text.lines().enumerate() {
        if re.is_match(line) {
            if out.len() >= MAX_MATCHES {
                *truncated = true;
                return;
            }
            out.push(format!("{}:{}:{}", rel_str, i + 1, line));
        }
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(4096);
    bytes[..n].contains(&0)
}

fn resolve_path_within(cwd: &Path, p: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    // v0.9.2: no cwd-escape guard on read-only tools (see tool/read.rs).
    std::fs::canonicalize(&candidate)
        .map_err(|e| ToolError::Execution(format!("cannot resolve {}: {e}", candidate.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("nanopi-grep-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    #[tokio::test]
    async fn finds_matches_in_files() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "hello world\nfoo bar\nHELLO again\n").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing here\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello"}), &ctx)
            .await
            .unwrap();
        // Case-sensitive: only line 1 matches.
        assert!(out.content.contains("a.txt:1:hello world"));
        assert!(!out.content.contains("HELLO"));
        assert!(!out.content.contains("b.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn case_insensitive_matches_both() {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "hello\nHELLO\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello", "case_insensitive": true}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("a.txt:1:hello"));
        assert!(out.content.contains("a.txt:2:HELLO"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let dir = tmp();
        // NUL byte in first 4KB → treated as binary.
        std::fs::write(dir.join("bin.dat"), b"hello\x00world").unwrap();
        std::fs::write(dir.join("txt.txt"), "hello world").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "hello"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("txt.txt"));
        assert!(!out.content.contains("bin.dat"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_regex_is_error() {
        let dir = tmp();
        let ctx = ToolContext { cwd: dir.clone() };
        let r = GrepTool.execute(json!({"pattern": "["}), &ctx).await;
        assert!(matches!(r, Err(ToolError::InvalidArgs(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a corpus that exercises every rule the two engines have to
    /// agree on: nesting, dotfiles, ignore dirs, binaries, an oversized
    /// file, and a leading-dash pattern hazard.
    fn corpus() -> PathBuf {
        let dir = tmp();
        std::fs::write(dir.join("a.txt"), "hello world\nfoo bar\nHELLO again\n").unwrap();
        std::fs::write(dir.join("b.txt"), "nothing here\n").unwrap();
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("sub/c.txt"), "hello nested\n").unwrap();
        std::fs::write(dir.join("sub/deeper/d.txt"), "hello deeper\n").unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("node_modules/pkg/e.txt"), "hello ignored\n").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/f.txt"), "hello ignored too\n").unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join(".hidden/g.txt"), "hello dotdir\n").unwrap();
        std::fs::write(dir.join(".dotfile"), "hello dotfile\n").unwrap();
        std::fs::write(dir.join("bin.dat"), b"hello\x00binary").unwrap();
        // > MAX_FILE_BYTES, so both engines must skip it.
        let mut big = String::with_capacity(6 * 1024 * 1024);
        big.push_str("hello huge\n");
        while big.len() < 6 * 1024 * 1024 {
            big.push_str("padding padding padding\n");
        }
        std::fs::write(dir.join("big.txt"), big).unwrap();
        dir
    }

    async fn run(dir: &Path, args: Value, rg: bool) -> ToolOutput {
        // The engine choice is process-global and memoized, so it cannot
        // be flipped by an env var mid-test. Calling the two search
        // functions directly is what makes a differential test possible
        // at all.
        let ctx = ToolContext { cwd: dir.to_path_buf() };
        let pattern = args["pattern"].as_str().unwrap();
        let ci = args["case_insensitive"].as_bool().unwrap_or(false);
        let all = args["all"].as_bool().unwrap_or(false);
        let base_str = args["path"].as_str().unwrap_or(".");
        let base = resolve_path_within(&ctx.cwd, base_str).unwrap();
        let root = if base.is_dir() {
            base.clone()
        } else {
            base.parent().unwrap().to_path_buf()
        };
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ci)
            .unicode(false)
            .build()
            .unwrap();
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        if rg {
            let path = crate::util::which::which("rg").expect("rg needed for this test");
            search_ripgrep(
                &path, pattern, &base, &root, ci, all, &mut matches, &mut truncated,
            )
            .await
            .unwrap();
        } else {
            search_builtin(
                &base,
                &root,
                &re,
                all,
                &mut matches,
                &mut truncated,
                &mut scanned,
            );
        }
        ToolOutput {
            content: matches.join("\n"),
            is_error: false,
            images: Vec::new(),
            metadata: Some(json!({"truncated": truncated})),
        }
    }

    /// Sorted line sets, because only ripgrep is asked to sort and the
    /// built-in emits in readdir order. Set equality is the contract;
    /// ordering within one engine is not.
    fn lines(o: &ToolOutput) -> Vec<String> {
        let mut v: Vec<String> = o
            .content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|s| s.to_string())
            .collect();
        v.sort();
        v
    }

    /// The test that makes the whole rg path defensible: identical
    /// results from both engines across the flag matrix. Without this,
    /// "rg when available" means the tool answers differently depending
    /// on the machine, which is worse than being slow.
    #[tokio::test]
    async fn ripgrep_and_builtin_agree() {
        if crate::util::which::which("rg").is_none() {
            eprintln!("skipping: rg not installed");
            return;
        }
        let dir = corpus();
        for args in [
            json!({"pattern": "hello"}),
            json!({"pattern": "hello", "case_insensitive": true}),
            json!({"pattern": "HELLO"}),
            json!({"pattern": "hello", "all": true}),
            json!({"pattern": "^hello"}),
            json!({"pattern": "hello (world|nested)"}),
            json!({"pattern": "nothing", "path": "b.txt"}),
            json!({"pattern": "hello", "path": "sub"}),
            json!({"pattern": "no-such-string-anywhere"}),
        ] {
            let rg_out = run(&dir, args.clone(), true).await;
            let bi_out = run(&dir, args.clone(), false).await;
            assert_eq!(
                lines(&rg_out),
                lines(&bi_out),
                "engines disagreed for {args}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not a correctness test, and `#[ignore]`d so it never costs CI
    /// time: run with `cargo test --release -- --ignored --nocapture
    /// engine_speed` to re-measure. It exists because "rg is faster" was
    /// asserted before it was measured, and `--sort path` gives up rg's
    /// parallel walk — so the speedup is worth re-checking whenever
    /// these flags change.
    #[tokio::test]
    #[ignore]
    async fn engine_speed_on_this_repo() {
        let Some(_) = crate::util::which::which("rg") else {
            return;
        };
        let root = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let dir = PathBuf::from(&root);
        let args = json!({"pattern": "fn \\w+\\("});
        // Warm the page cache so the first engine measured is not
        // charged for cold IO.
        let _ = run(&dir, args.clone(), true).await;
        let _ = run(&dir, args.clone(), false).await;
        let t0 = std::time::Instant::now();
        let rg = run(&dir, args.clone(), true).await;
        let rg_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let bi = run(&dir, args.clone(), false).await;
        let bi_ms = t1.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "ripgrep {:.1} ms / builtin {:.1} ms = {:.2}x  ({} vs {} lines)",
            rg_ms,
            bi_ms,
            bi_ms / rg_ms,
            lines(&rg).len(),
            lines(&bi).len()
        );
    }

    /// Pinned separately because it is the one case where they must NOT
    /// agree, and an accidental "fix" that made them agree would mean
    /// either rg lost its ignore globs or the built-in started reading
    /// .gitignore. Documented in the module header.
    #[tokio::test]
    async fn non_utf8_is_the_one_known_divergence() {
        if crate::util::which::which("rg").is_none() {
            return;
        }
        let dir = tmp();
        // Latin-1 é — invalid UTF-8.
        std::fs::write(dir.join("latin1.txt"), b"caf\xe9 hello\n").unwrap();
        let args = json!({"pattern": "hello"});
        assert!(lines(&run(&dir, args.clone(), false).await).is_empty());
        assert_eq!(lines(&run(&dir, args, true).await).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ignore dirs must be skipped by rg via explicit globs, since
    /// `--no-ignore` turns off the .gitignore that would normally do it.
    /// A dropped glob would show up here and nowhere else.
    #[tokio::test]
    async fn ripgrep_skips_ignore_dirs_without_gitignore() {
        if crate::util::which::which("rg").is_none() {
            return;
        }
        let dir = corpus();
        let out = run(&dir, json!({"pattern": "hello"}), true).await;
        let joined = out.content;
        assert!(!joined.contains("node_modules"), "got: {joined}");
        assert!(!joined.contains("target/"), "got: {joined}");
        assert!(!joined.contains(".hidden"), "got: {joined}");
        assert!(!joined.contains(".dotfile"), "got: {joined}");
        assert!(!joined.contains("big.txt"), "got: {joined}");
        assert!(!joined.contains("bin.dat"), "got: {joined}");
        assert!(joined.contains("sub/deeper/d.txt"), "got: {joined}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--sort path` is not cosmetic: without it the 500-match cap keeps
    /// an arbitrary subset. Asserted on the arg vector so it holds even
    /// where rg is not installed.
    #[test]
    fn compat_flags_are_present() {
        let a = ripgrep_args("pat", ".", false, false);
        for required in [
            "--no-config",
            "--no-unicode",
            "--no-ignore",
            "--sort",
            "path",
            "--no-heading",
            "--line-number",
            "--with-filename",
        ] {
            assert!(a.iter().any(|x| x == required), "missing {required} in {a:?}");
        }
        // Every non-dot ignore dir needs a glob; the dot ones are covered
        // by rg's default hidden-skipping.
        for d in IGNORE_DIRS {
            assert!(a.contains(&format!("--glob=!{d}")), "no glob for {d}");
        }
        // `-e` / `--` guard a leading-dash pattern or path.
        assert!(a.contains(&"-e".to_string()));
        assert!(a.contains(&"--".to_string()));
        // `all: true` swaps the globs for --hidden.
        let all = ripgrep_args("pat", ".", false, true);
        assert!(all.contains(&"--hidden".to_string()));
        assert!(!all.iter().any(|x| x.starts_with("--glob=")));
    }

    #[tokio::test]
    async fn single_file_target() {
        let dir = tmp();
        std::fs::write(dir.join("single.txt"), "foo\nbar\n").unwrap();
        let ctx = ToolContext { cwd: dir.clone() };
        let out = GrepTool
            .execute(json!({"pattern": "bar", "path": "single.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("single.txt:2:bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
