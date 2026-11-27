//! iTerm2 inline images: detection and framing.
//!
//! Detection is decided once, at startup, before the TUI reads the
//! keyboard: an explicit `NANOPI_INLINE_IMAGES` wins; then local hints
//! (`NEXUSTERM_INLINE_IMAGES`, `TERM_PROGRAM`, `LC_TERMINAL`); then the
//! terminal is asked its name (XTVERSION, with a DA1 query behind it so
//! the answer's end is known without guessing). That last step is what
//! makes it work over SSH, where environment variables do not travel.

use std::ffi::OsString;

use base64::{engine::general_purpose::STANDARD, Engine as _};

/// The nanopi logo (535x160), drawn at startup and by `/image-test`.
/// Regenerate with `assets/make_logo.py`.
pub(super) const LOGO_PNG: &[u8] = include_bytes!("../../assets/nanopi-logo.png");
const MAX_RAW_BYTES: usize = 3_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Support {
    Iterm2,
    Off(&'static str),
}

pub(super) fn detect(mut env: impl FnMut(&str) -> Option<OsString>, is_tty: bool) -> Support {
    match env("NANOPI_INLINE_IMAGES")
        .as_deref()
        .and_then(|s| s.to_str())
    {
        Some("iterm2") => return Support::Iterm2,
        Some("off") => return Support::Off("disabled by NANOPI_INLINE_IMAGES=off"),
        _ => {}
    }
    if !is_tty {
        return Support::Off("stdout is not a TTY");
    }
    if env("TMUX").is_some() {
        return Support::Off("tmux may drop or garble inline images");
    }
    if env("NEXUSTERM_INLINE_IMAGES").is_some_and(|s| s == "iterm2")
        || env("TERM_PROGRAM").is_some_and(|s| s == "iTerm.app" || s == "WezTerm")
        || env("LC_TERMINAL").is_some_and(|s| s == "iTerm2")
    {
        Support::Iterm2
    } else {
        Support::Off(NOT_ADVERTISED)
    }
}

/// Terminals that answer XTVERSION with one of these names draw iTerm2
/// inline images. (tmux answers with its own name, and stays off.)
const IMAGE_TERMINALS: [&str; 4] = ["iterm2", "wezterm", "nexusterm", "mintty"];

/// The terminal's name from an XTVERSION reply (`ESC P > | name ESC \`),
/// control characters removed, at most 64 characters.
#[cfg_attr(not(unix), allow(dead_code))] // the terminal query is Unix-only
pub(super) fn parse_xtversion(reply: &[u8]) -> Option<String> {
    let start = reply.windows(4).position(|w| w == b"\x1bP>|")? + 4;
    let rest = &reply[start..];
    let end = rest
        .iter()
        .position(|&b| b == 0x1b || b == 0x07)
        .unwrap_or(rest.len());
    let name: String = String::from_utf8_lossy(&rest[..end])
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Whether `reply` contains the DA1 answer (`ESC [ ? … c`), which every
/// terminal sends: once it is in, any XTVERSION answer is too.
#[cfg_attr(not(unix), allow(dead_code))] // the terminal query is Unix-only
pub(super) fn da1_seen(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(i, w)| {
        w == b"\x1b[?"
            && reply[i + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                .is_some_and(|&b| b == b'c')
    })
}

pub(super) fn name_draws_images(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    IMAGE_TERMINALS.iter().any(|t| name.starts_with(t))
}

/// Ask the terminal its name. Unix only; `None` when it does not say
/// within `timeout` (most terminals answer in a few milliseconds).
#[cfg(unix)]
fn query_terminal_name(timeout: std::time::Duration) -> Option<String> {
    use std::io::Write as _;
    use std::os::fd::AsRawFd as _;
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw && crossterm::terminal::enable_raw_mode().is_err() {
        return None;
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[>0q\x1b[c");
    let _ = out.flush();
    let fd = std::io::stdin().as_raw_fd();
    let deadline = std::time::Instant::now() + timeout;
    let mut reply = Vec::new();
    while !da1_seen(&reply) && reply.len() < 4096 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for a borrowed, open descriptor.
        let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, 1000) as i32) };
        if ready <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        // SAFETY: reads at most chunk.len() bytes into an owned buffer.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        reply.extend_from_slice(&chunk[..n as usize]);
    }
    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    parse_xtversion(&reply)
}

#[cfg(not(unix))]
fn query_terminal_name(_timeout: std::time::Duration) -> Option<String> {
    None
}

static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();

/// Decide image support once, asking the terminal when nothing else
/// says. Call before the TUI starts reading input. The result is cached;
/// [`support`] returns it afterwards.
pub(super) fn init_support() -> &'static Support {
    SUPPORT.get_or_init(|| {
        let env = |key: &str| std::env::var_os(key);
        let tty = std::io::IsTerminal::is_terminal(&std::io::stdout())
            && std::io::IsTerminal::is_terminal(&std::io::stdin());
        match detect(env, tty) {
            Support::Off(NOT_ADVERTISED) => {
                // Over SSH the answer takes a round trip.
                let ssh = env("SSH_CONNECTION").is_some() || env("SSH_TTY").is_some();
                let wait = std::time::Duration::from_millis(if ssh { 500 } else { 150 });
                match query_terminal_name(wait) {
                    Some(name) if name_draws_images(&name) => Support::Iterm2,
                    _ => Support::Off(NOT_ADVERTISED),
                }
            }
            decided => decided,
        }
    })
}

/// The support decided at startup (or, if nothing decided it yet, the
/// environment's answer without asking the terminal).
pub(super) fn support() -> Support {
    match SUPPORT.get() {
        Some(s) => s.clone(),
        None => detect(|key| std::env::var_os(key), std::io::IsTerminal::is_terminal(&std::io::stdout())),
    }
}

const NOT_ADVERTISED: &str = "terminal did not identify as one that draws inline images";

/// Largest file `/image` sends (the terminal's own limit is 4 MB).
pub(super) const MAX_FILE_BYTES: u64 = MAX_RAW_BYTES as u64;

/// Formats every iTerm2-protocol terminal nanopi targets can decode.
/// xterm.js's image addon (NexusTerm, VS Code) reads PNG, JPEG and GIF
/// only, so WebP is refused here rather than sent and silently dropped.
pub(super) fn displayable(media_type: &str) -> bool {
    matches!(media_type, "image/png" | "image/jpeg" | "image/gif")
}

/// Pixel size of a PNG, JPEG or GIF, read from its header. `None` when
/// the header does not say (the caller then assumes a 2:1 picture).
pub(super) fn dimensions(d: &[u8]) -> Option<(u32, u32)> {
    let be16 = |i: usize| Some(u32::from(*d.get(i)?) << 8 | u32::from(*d.get(i + 1)?));
    let size = if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        let be32 = |i: usize| Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?));
        (be32(16)?, be32(20)?)
    } else if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") {
        let le16 = |i: usize| Some(u32::from(*d.get(i)?) | u32::from(*d.get(i + 1)?) << 8);
        (le16(6)?, le16(8)?)
    } else if d.starts_with(&[0xFF, 0xD8]) {
        // Walk the segments to the first start-of-frame marker.
        let mut i = 2;
        loop {
            if *d.get(i)? != 0xFF {
                return None;
            }
            let marker = *d.get(i + 1)?;
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                break (be16(i + 7)?, be16(i + 5)?);
            }
            i += 2 + usize::try_from(be16(i + 2)?).ok()?;
        }
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// Box (cells wide, rows tall) for a picture of `pixels` above a dock of
/// `dock_rows`: about 8 px per cell so small images are not blown up,
/// at most 64 cells wide, the aspect kept with cells ~twice as tall as
/// wide, and never taller than the space above the dock.
pub(super) fn image_box(cols: u16, rows: u16, dock_rows: u16, pixels: Option<(u32, u32)>) -> Option<(u16, u16)> {
    let (pw, ph) = pixels.unwrap_or((2, 1));
    let max_w = cols.saturating_sub(4).min(64);
    let available = rows.saturating_sub(dock_rows).min(24);
    if max_w < 4 || available == 0 {
        return None;
    }
    let natural = u16::try_from(pw.div_ceil(8)).unwrap_or(u16::MAX);
    let mut width = natural.clamp(4, max_w);
    let rows_for = |w: u16| -> u16 {
        let r = (u64::from(w) * u64::from(ph)).div_ceil(2 * u64::from(pw));
        u16::try_from(r).unwrap_or(u16::MAX).max(1)
    };
    // Too tall for the space: narrow it until it fits (aspect kept).
    if rows_for(width) > available {
        let w = (u64::from(available) * 2 * u64::from(pw)) / u64::from(ph).max(1);
        width = u16::try_from(w).unwrap_or(u16::MAX).clamp(1, max_w);
    }
    Some((width, rows_for(width).min(available)))
}

pub(super) fn encode(data: &[u8], width: u16, height: u16) -> anyhow::Result<String> {
    anyhow::ensure!(
        data.len() <= MAX_RAW_BYTES,
        "inline image exceeds 3,000,000 bytes"
    );
    Ok(format!(
        "\x1b]1337;File=inline=1;size={};width={width};height={height};preserveAspectRatio=1:{}\x07",
        data.len(),
        STANDARD.encode(data),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_precedence() {
        let support = |vars: &[(&str, &str)], tty| {
            detect(
                |key| {
                    vars.iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, v)| (*v).into())
                },
                tty,
            )
        };
        for (name, value) in [
            ("NEXUSTERM_INLINE_IMAGES", "iterm2"),
            ("TERM_PROGRAM", "iTerm.app"),
            ("LC_TERMINAL", "iTerm2"),
            ("TERM_PROGRAM", "WezTerm"),
        ] {
            assert_eq!(support(&[(name, value)], true), Support::Iterm2);
            assert_eq!(
                support(&[(name, value)], false),
                Support::Off("stdout is not a TTY")
            );
            assert_eq!(
                support(&[(name, value), ("TMUX", "")], true),
                Support::Off("tmux may drop or garble inline images")
            );
            assert_eq!(
                support(
                    &[
                        (name, value),
                        ("TMUX", "1"),
                        ("NANOPI_INLINE_IMAGES", "off")
                    ],
                    false
                ),
                Support::Off("disabled by NANOPI_INLINE_IMAGES=off")
            );
        }
        assert_eq!(
            support(&[("NANOPI_INLINE_IMAGES", "iterm2"), ("TMUX", "1")], false),
            Support::Iterm2
        );
        assert_eq!(
            support(
                &[
                    ("NANOPI_INLINE_IMAGES", "unknown"),
                    ("TERM_PROGRAM", "WezTerm")
                ],
                true
            ),
            Support::Iterm2
        );
        assert_eq!(
            support(&[("TERM", "xterm-256color")], true),
            Support::Off(NOT_ADVERTISED)
        );
    }

    #[test]
    fn encoder_frames_raw_byte_count_and_round_trips() {
        let input = b"\0\n\xffbanana";
        let sequence = encode(input, 24, 6).unwrap();
        assert!(!sequence.contains('\n'));
        let payload = sequence
            .strip_prefix("\x1b]1337;File=inline=1;size=9;width=24;height=6;preserveAspectRatio=1:")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(STANDARD.decode(payload).unwrap(), input);
    }

    #[test]
    fn dimensions_come_from_the_header() {
        assert_eq!(dimensions(LOGO_PNG), Some((535, 160)));
        assert_eq!(dimensions(b"GIF89a\x40\x01\xf0\x00rest"), Some((320, 240)));
        // JPEG: SOI, an APP0 segment, then SOF0 with height 0x01E0, width 0x0280.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, b'J', b'F',
            0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03,
        ];
        assert_eq!(dimensions(&jpeg), Some((640, 480)));
        assert_eq!(dimensions(&jpeg[..12]), None, "cut short");
        assert_eq!(dimensions(b"not an image at all"), None);
        assert!(displayable("image/png") && displayable("image/jpeg") && displayable("image/gif"));
        assert!(!displayable("image/webp"));
    }

    #[test]
    fn image_box_keeps_aspect_and_fits_above_the_dock() {
        // Natural size: 96 px wide is 12 cells, 2:1 is 3 rows.
        assert_eq!(image_box(200, 50, 10, Some((96, 48))), Some((12, 3)));
        // Wide screenshot: capped at 64 cells.
        assert_eq!(image_box(200, 50, 10, Some((1920, 1080))), Some((64, 18)));
        // Tall image narrowed to fit the space above the dock.
        let (w, h) = image_box(200, 20, 10, Some((400, 1600))).unwrap();
        assert!(h <= 10 && w < 64, "{w}x{h}");
        // Tiny images are not shrunk below 4 cells; unknown size is 2:1.
        assert_eq!(image_box(200, 50, 10, Some((8, 8))), Some((4, 2)));
        assert_eq!(image_box(200, 50, 10, None), Some((4, 1)));
        assert_eq!(image_box(7, 50, 10, Some((96, 48))), None);
        assert_eq!(image_box(80, 10, 10, Some((96, 48))), None);
    }

    /// The terminal's answers: its name from XTVERSION, the end marked by
    /// DA1. iTerm2, WezTerm, NexusTerm and mintty draw images; tmux and
    /// unknown terminals do not.
    #[test]
    fn terminal_answers_are_parsed() {
        let reply = b"\x1bP>|NexusTerm 0.9.1\x1b\\\x1b[?62;22c";
        assert_eq!(parse_xtversion(reply).as_deref(), Some("NexusTerm 0.9.1"));
        assert!(da1_seen(reply));
        assert!(!da1_seen(b"\x1bP>|NexusTerm 0.9.1\x1b\\"), "XTVERSION alone is not the end");
        assert!(!da1_seen(b"\x1b[?62;2"), "DA1 cut short");
        assert_eq!(parse_xtversion(b"\x1b[?1;2c"), None, "terminal without XTVERSION");
        assert_eq!(parse_xtversion(b"\x1bP>|iTerm2 3.5.0\x07").as_deref(), Some("iTerm2 3.5.0"));
        assert_eq!(parse_xtversion(b"\x1bP>|evil\x1b[2J\x1b\\").as_deref(), Some("evil"));
        for name in ["iTerm2 3.5.0", "WezTerm 20240203", "NexusTerm 0.9.1", "mintty 3.7.0"] {
            assert!(name_draws_images(name), "{name}");
        }
        for name in ["tmux 3.4", "XTerm(390)", "foot(1.18.0)", ""] {
            assert!(!name_draws_images(name), "{name}");
        }
    }

    #[test]
    fn oversized_data_is_rejected() {
        assert!(encode(&vec![0; MAX_RAW_BYTES + 1], 24, 6).is_err());
    }

}
