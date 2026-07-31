//! Image file policy shared by REPL `@path` attachments and the `view_image`
//! tool: the per-image size cap, magic-number type detection, and
//! file → `data:` URL reading.

use std::path::Path;

use base64::Engine as _;

/// Per-image size limit when a model does not set `max_image_bytes` (matches
/// the Anthropic API's 5 MB per-image cap; a clear local error beats a
/// confusing provider 400).
///
/// Measured on the **base64 payload**, not the file on disk: images travel as
/// `data:` URLs, and base64 inflates by 4/3, so a 5 MiB file is already a
/// 6.7 MiB upload. Checking the file size instead understates every image by a
/// third and lets rejects through to the provider.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Size of `bytes` once base64-encoded — what a request actually carries.
pub fn base64_len(bytes: u64) -> u64 {
    bytes.div_ceil(3).saturating_mul(4)
}

/// Render a byte count as MiB for user-facing limits and sizes. One decimal
/// throughout: a configured cap is an arbitrary number of bytes, and integer
/// MiB would round `3_500_000` down to a "3 MiB" limit that rejects at 3.3.
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Media types the model APIs accept. `infer` recognizes far more formats
/// than this, so detection is filtered down to what we can actually send.
const SUPPORTED_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Whether a path *looks* like an image by extension.
///
/// Only for deciding which REPL `@token`s are attachment mentions — that call
/// happens before the file is read. The media type actually sent to the
/// provider always comes from the bytes ([`read_image_data_url`]).
pub fn looks_like_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

/// Read an image file and encode it as a `data:` URL, rejecting anything whose
/// base64 payload exceeds `max_bytes` (the driving model's cap — see
/// [`DEFAULT_MAX_IMAGE_BYTES`]).
///
/// The media type comes from the file's magic number, not its name: a `.png`
/// that is really a JPEG would otherwise reach the provider tagged
/// `image/png` and be rejected by a decoder we cannot see. `label` is how the
/// path appears in error messages (the REPL uses `@path`, `view_image` quotes
/// it).
pub fn read_image_data_url(path: &Path, label: &str, max_bytes: u64) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read image {label}: {e}"))?;
    let encoded = base64_len(meta.len());
    if encoded > max_bytes {
        return Err(format!(
            "image {label} is {} ({} encoded for upload); the limit is {}. \
             Resize or re-compress it and resubmit",
            mib(meta.len()),
            mib(encoded),
            mib(max_bytes),
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read image {label}: {e}"))?;
    let media_type = infer::get(&bytes)
        .map(|kind| kind.mime_type())
        .filter(|mime| SUPPORTED_MEDIA_TYPES.contains(mime))
        .ok_or_else(|| format!("{label} is not a png, jpeg, gif, or webp image"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{media_type};base64,{data}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, temp_dir};

    /// Smallest byte strings `infer`'s matchers accept, per format.
    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    const GIF: &[u8] = b"GIF89a";
    const WEBP: &[u8] = b"RIFF\x00\x00\x00\x00WEBP";

    fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn extension_filter_is_case_insensitive_and_rejects_non_images() {
        assert!(looks_like_image_path("shot.png"));
        assert!(looks_like_image_path("photo.JPG"));
        assert!(looks_like_image_path("/a/b/anim.gif"));
        assert!(looks_like_image_path("x.webp"));
        assert!(!looks_like_image_path("notes.txt"));
        assert!(!looks_like_image_path("alice"));
    }

    #[test]
    fn media_type_comes_from_bytes_for_every_supported_format() {
        let dir = temp_dir("image");
        for (name, bytes, want) in [
            ("a.png", PNG, "image/png"),
            ("b.jpg", JPEG, "image/jpeg"),
            ("c.gif", GIF, "image/gif"),
            ("d.webp", WEBP, "image/webp"),
        ] {
            let path = write(&dir, name, bytes);
            let url = read_image_data_url(&path, name, DEFAULT_MAX_IMAGE_BYTES).unwrap();
            assert!(url.starts_with(&format!("data:{want};base64,")), "{url}");
        }
    }

    /// The reason we sniff: the extension can lie, and a wrong `media_type`
    /// on the wire is a provider-side decode error we cannot diagnose.
    #[test]
    fn mislabeled_extension_uses_the_real_media_type() {
        let dir = temp_dir("image");
        let path = write(&dir, "actually-a-jpeg.png", JPEG);
        let url =
            read_image_data_url(&path, "@actually-a-jpeg.png", DEFAULT_MAX_IMAGE_BYTES).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
    }

    #[test]
    fn round_trips_the_bytes() {
        let dir = temp_dir("image");
        let path = write(&dir, "a.png", PNG);
        let url = read_image_data_url(&path, "a.png", DEFAULT_MAX_IMAGE_BYTES).unwrap();
        let b64 = url.strip_prefix("data:image/png;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(decoded, PNG);
    }

    /// A recognized non-image (PDF) and unrecognized bytes both fail the same
    /// way: only the four accepted media types get through.
    #[test]
    fn non_image_content_is_rejected() {
        let dir = temp_dir("image");
        for (name, bytes) in [
            ("doc.png", &b"%PDF-1.7\n"[..]),
            ("notes.png", &b"just text"[..]),
            ("empty.png", &b""[..]),
        ] {
            let path = write(&dir, name, bytes);
            let err = read_image_data_url(&path, name, DEFAULT_MAX_IMAGE_BYTES).unwrap_err();
            assert!(
                err.contains("is not a png, jpeg, gif, or webp image"),
                "{err}"
            );
        }
    }

    /// Size is checked from metadata before the read, so an oversized file is
    /// never pulled into memory just to be rejected.
    #[test]
    fn oversized_image_errors_with_limit() {
        let dir = temp_dir("image");
        let path = write(
            &dir,
            "big.png",
            &vec![0u8; DEFAULT_MAX_IMAGE_BYTES as usize + 1],
        );
        let err = read_image_data_url(&path, "@big.png", DEFAULT_MAX_IMAGE_BYTES).unwrap_err();
        assert!(err.contains("the limit is 5.0 MiB"), "{err}");
    }

    /// The cap is the caller's, not a constant: the same file passes under a
    /// model configured for larger images and fails under a smaller one.
    #[test]
    fn limit_comes_from_the_caller() {
        let dir = temp_dir("image");
        // 4 MiB file → ~5.3 MiB encoded: over the 5 MiB default, under 8 MiB.
        let mut bytes = vec![0u8; 4 * 1024 * 1024];
        bytes[..PNG.len()].copy_from_slice(PNG);
        let path = write(&dir, "shot.png", &bytes);

        let url = read_image_data_url(&path, "@shot.png", 8 * 1024 * 1024).unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");

        let err = read_image_data_url(&path, "@shot.png", 1024 * 1024).unwrap_err();
        assert!(err.contains("the limit is 1.0 MiB"), "{err}");
    }

    /// The cap is on the uploaded payload, not the file: base64 inflates by
    /// 4/3, so a 4 MiB file is already over a 5 MiB encoded limit. Checking
    /// the file size instead would wave this one through to the provider.
    #[test]
    fn limit_is_measured_on_the_base64_payload() {
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);

        let dir = temp_dir("image");
        let path = write(&dir, "shot.png", &vec![0u8; 4 * 1024 * 1024]);
        let err = read_image_data_url(&path, "@shot.png", DEFAULT_MAX_IMAGE_BYTES).unwrap_err();
        assert!(err.contains("5.3 MiB encoded"), "{err}");
    }

    #[test]
    fn missing_file_errors_naming_the_label() {
        let err = read_image_data_url(
            Path::new("/definitely/missing.png"),
            "@missing.png",
            DEFAULT_MAX_IMAGE_BYTES,
        )
        .unwrap_err();
        assert!(err.contains("@missing.png"), "{err}");
    }
}
