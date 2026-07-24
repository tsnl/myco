//! Image file policy shared by REPL `@path` attachments and the editor's
//! `view`: which extensions count as images, the per-image size cap, and
//! file → `data:` URL reading.

use std::path::Path;

use base64::Engine as _;

/// Per-image size limit (matches the Anthropic API's 5 MB per-image cap; a
/// clear local error beats a confusing provider 400).
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Media type for a path with a supported image extension, else `None`.
pub fn image_media_type(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Read an image file and encode it as a `data:` URL, enforcing
/// [`MAX_IMAGE_BYTES`]. `label` is how the path appears in error messages
/// (the REPL uses `@path`, the editor quotes it).
pub fn image_file_data_url(path: &Path, media_type: &str, label: &str) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read image {label}: {e}"))?;
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image {label} is {:.1} MiB; the limit is {} MiB",
            meta.len() as f64 / (1024.0 * 1024.0),
            MAX_IMAGE_BYTES / (1024 * 1024),
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read image {label}: {e}"))?;
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{media_type};base64,{data}"))
}
