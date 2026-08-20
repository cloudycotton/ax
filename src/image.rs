//! Turning what the agent produces into something a model can look at.
//!
//! Providers accept images as data URIs inside a message's content parts, so
//! the work here is recognising the format, enforcing a size the endpoint will
//! actually accept, and encoding. Screenshots arrive already base64-encoded
//! from CDP, files arrive as bytes; both end up in the same shape.

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Raw bytes above this are refused. Providers vary, but every one of them
/// rejects something eventually, and a 20MB screenshot is never the right way
/// to answer a question.
const MAX_BYTES: usize = 5 * 1024 * 1024;

/// Identify an image by its magic bytes rather than trusting a file extension.
pub fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        // RIFF....WEBP
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

/// Build a `data:` URI from raw image bytes.
pub fn data_uri(bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        bail!("the image is empty");
    }
    if bytes.len() > MAX_BYTES {
        bail!(
            "the image is {:.1}MB, over the {}MB limit — scale it down first, \
e.g. `sips -Z 1400 shot.png` on macOS",
            bytes.len() as f64 / 1_048_576.0,
            MAX_BYTES / 1_048_576
        );
    }
    let Some(mime) = detect_mime(bytes) else {
        bail!("not a recognisable PNG, JPEG, GIF, or WebP image");
    };
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

/// Accept what the agent is likely to have in hand: a path, a bare base64
/// string (what `page.screenshot()` returns), or an already-formed data URI.
pub fn resolve(input: &str, resolve_path: impl Fn(&str) -> std::path::PathBuf) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("see() needs a file path, a base64 image, or a data: URI");
    }

    if trimmed.starts_with("data:image/") {
        return Ok(trimmed.to_string());
    }

    // A path wins when the file exists, so a filename that happens to look
    // like base64 still behaves the way anyone would expect.
    let path = resolve_path(trimmed);
    if path.is_file() {
        let bytes = std::fs::read(&path)
            .map_err(|err| anyhow::anyhow!("could not read {}: {err}", path.display()))?;
        return data_uri(&bytes);
    }

    // Otherwise treat it as base64. Note that `/` is part of the base64
    // alphabet, so it says nothing about whether this is a path — any real
    // screenshot contains several. Decide by whether it decodes into something
    // that is actually an image, and fall back to reporting a missing file so
    // a mistyped filename gets a sensible message rather than a codec error.
    if trimmed.len() > 64
        && !trimmed.contains(char::is_whitespace)
        && let Ok(bytes) = STANDARD.decode(trimmed)
        && detect_mime(&bytes).is_some()
    {
        return data_uri(&bytes);
    }

    bail!("no file at {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The smallest valid PNG: an 1x1 transparent pixel.
    fn tiny_png() -> Vec<u8> {
        STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==")
            .unwrap()
    }

    #[test]
    fn detects_formats_by_magic_bytes() {
        assert_eq!(detect_mime(&tiny_png()), Some("image/png"));
        assert_eq!(
            detect_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            Some("image/jpeg")
        );
        assert_eq!(detect_mime(b"GIF89a....."), Some("image/gif"));
        assert_eq!(detect_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(detect_mime(b"not an image at all"), None);
    }

    #[test]
    fn builds_a_data_uri() {
        let uri = data_uri(&tiny_png()).unwrap();
        assert!(uri.starts_with("data:image/png;base64,iVBOR"), "{uri}");
    }

    #[test]
    fn refuses_non_images_and_empty_input() {
        assert!(data_uri(b"").is_err());
        assert!(data_uri(b"<html></html>").is_err());
    }

    #[test]
    fn refuses_oversized_images() {
        // A valid PNG header followed by more bytes than any provider wants.
        let mut huge = tiny_png();
        huge.resize(MAX_BYTES + 1, 0);
        let err = data_uri(&huge).unwrap_err().to_string();
        assert!(err.contains("over the"), "{err}");
    }

    #[test]
    fn passes_through_an_existing_data_uri() {
        let uri = "data:image/png;base64,AAAA";
        assert_eq!(resolve(uri, |s: &str| PathBuf::from(s)).unwrap(), uri);
    }

    #[test]
    fn accepts_bare_base64_from_a_screenshot() {
        let encoded = STANDARD.encode(tiny_png());
        let uri = resolve(&encoded, |s: &str| PathBuf::from(s)).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn accepts_base64_containing_slashes() {
        // `/` is in the base64 alphabet, so real screenshots are full of them.
        // Treating a slash as proof of a path broke every page.screenshot().
        let mut bytes = tiny_png();
        bytes.extend(std::iter::repeat_n(0xFF, 128));
        let encoded = STANDARD.encode(&bytes);
        assert!(encoded.contains('/'), "test data must exercise the bug");

        let uri = resolve(&encoded, |s: &str| PathBuf::from(s)).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn reports_a_missing_file_rather_than_a_base64_error() {
        let err = resolve("shot.png", |s: &str| PathBuf::from(s))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file at"), "{err}");
    }

    #[test]
    fn reads_an_image_from_disk() {
        let dir = std::env::temp_dir().join("ax-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pixel.png");
        std::fs::write(&path, tiny_png()).unwrap();

        let uri = resolve(path.to_str().unwrap(), |s: &str| PathBuf::from(s)).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
