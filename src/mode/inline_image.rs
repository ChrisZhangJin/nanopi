//! Passive iTerm2 inline-image detection and framing. No terminal queries.

use std::ffi::OsString;

use base64::{engine::general_purpose::STANDARD, Engine as _};

pub(super) const TEST_PNG: &[u8] = include_bytes!("../../assets/image-test.png");
const MAX_RAW_BYTES: usize = 3_000_000;

#[derive(Debug, PartialEq, Eq)]
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
        Support::Off("terminal did not advertise iTerm2 inline images")
    }
}

/// Test image is 2:1, and a cell is roughly twice as tall as it is wide.
/// Keep the box entirely on screen, above the dock, with horizontal margin.
pub(super) fn test_box(cols: u16, rows: u16, dock_rows: u16) -> Option<(u16, u16)> {
    let width = cols.saturating_sub(4).min(48);
    let available = rows.saturating_sub(dock_rows);
    if width < 4 || available == 0 {
        return None;
    }
    Some((width, width.div_ceil(4).clamp(4, 12).min(available)))
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
            Support::Off("terminal did not advertise iTerm2 inline images")
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
    fn oversized_data_is_rejected() {
        assert!(encode(&vec![0; MAX_RAW_BYTES + 1], 24, 6).is_err());
    }

    #[test]
    fn box_respects_margin_aspect_and_available_rows() {
        assert_eq!(test_box(200, 50, 10), Some((48, 12)));
        assert_eq!(test_box(29, 50, 10), Some((25, 7)));
        assert_eq!(test_box(8, 50, 10), Some((4, 4)));
        assert_eq!(test_box(80, 11, 10), Some((48, 1)));
        assert_eq!(test_box(7, 50, 10), None);
        assert_eq!(test_box(80, 10, 10), None);
        assert_eq!(test_box(0, 0, 10), None);
    }
}
