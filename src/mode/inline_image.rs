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
    fn dimensions_come_from_the_header() {
        assert_eq!(dimensions(TEST_PNG), Some((96, 48)));
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
