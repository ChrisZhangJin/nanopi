//! Magic-byte detection of common image formats, matching PI's
//! `detectSupportedImageMimeType` (see
//! `packages/agent/src/harness/tools/image.ts:1-10`).
//!
//! We do NOT trust file extensions — a `.jpg` file with PNG bytes is
//! served as PNG. This matches Anthropic's tolerance and keeps
//! read-tool behavior predictable for users who have mislabeled files.
//!
//! Formats supported by nanopi's initial vision port:
//!   - PNG (with IHDR chunk sanity check; rejects animated PNGs)
//!   - JPEG (skips the JPEG-LS `0xF7` marker byte which vision models
//!     don't accept)
//!   - GIF (static and animated share a magic prefix; the model will
//!     read the first frame)
//!   - WebP
//!
//! BMP is intentionally omitted for MVP — Anthropic accepts it but
//! users rarely paste BMPs into a coding-agent conversation.

/// Return the Anthropic-canonical `media_type` string if `bytes`
/// starts with a recognized image signature. Returns `None` when the
/// bytes don't look like any supported image.
pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    if is_png(bytes) {
        Some("image/png")
    } else if is_jpeg(bytes) {
        Some("image/jpeg")
    } else if is_gif(bytes) {
        Some("image/gif")
    } else if is_webp(bytes) {
        Some("image/webp")
    } else {
        None
    }
}

fn is_png(bytes: &[u8]) -> bool {
    // PNG signature is 8 bytes, followed by a 4-byte length + 4-byte
    // chunk type. First chunk must be "IHDR" for a valid PNG — this
    // rejects APNG (still starts with the PNG signature but next
    // chunk is "acTL" not "IHDR" — mirrors PI's animated-PNG guard).
    const PNG_SIG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 16 || !bytes.starts_with(PNG_SIG) {
        return false;
    }
    &bytes[12..16] == b"IHDR"
}

fn is_jpeg(bytes: &[u8]) -> bool {
    // JPEG starts with 0xFF 0xD8 0xFF <marker>. The <marker> byte
    // 0xF7 identifies JPEG-LS (lossless variant Anthropic rejects);
    // exclude that prefix.
    bytes.len() >= 4
        && bytes[0] == 0xFF
        && bytes[1] == 0xD8
        && bytes[2] == 0xFF
        && bytes[3] != 0xF7
}

fn is_gif(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

fn is_webp(bytes: &[u8]) -> bool {
    // WebP is a RIFF container: "RIFF" + 4-byte size + "WEBP".
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_png_with_ihdr() {
        // Minimal valid PNG header: sig + length + "IHDR".
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length = 13
        bytes.extend_from_slice(b"IHDR");
        assert_eq!(detect_media_type(&bytes), Some("image/png"));
    }

    #[test]
    fn rejects_png_signature_with_wrong_chunk() {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 8]);
        bytes.extend_from_slice(b"acTL"); // animated PNG chunk
        assert!(detect_media_type(&bytes).is_none());
    }

    #[test]
    fn detects_jpeg() {
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(detect_media_type(&bytes), Some("image/jpeg"));
    }

    #[test]
    fn rejects_jpeg_ls() {
        // JPEG-LS: 0xFF 0xD8 0xFF 0xF7 — Anthropic doesn't accept it.
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xF7];
        assert!(detect_media_type(&bytes).is_none());
    }

    #[test]
    fn detects_gif_both_versions() {
        assert_eq!(detect_media_type(b"GIF87a...."), Some("image/gif"));
        assert_eq!(detect_media_type(b"GIF89a...."), Some("image/gif"));
    }

    #[test]
    fn detects_webp() {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 100]); // size
        bytes.extend_from_slice(b"WEBP");
        assert_eq!(detect_media_type(&bytes), Some("image/webp"));
    }

    #[test]
    fn rejects_non_image_bytes() {
        assert!(detect_media_type(b"just some text").is_none());
        assert!(detect_media_type(b"").is_none());
        assert!(detect_media_type(&[0x00; 20]).is_none());
    }

    #[test]
    fn magic_bytes_beat_extension() {
        // A "png" file that's actually a JPEG in disguise → JPEG.
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(detect_media_type(&bytes), Some("image/jpeg"));
    }
}
