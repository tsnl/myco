//! Image attachments in REPL input.
//!
//! A whitespace-delimited `@<path>` token whose extension is a supported image
//! type (png, jpg, jpeg, gif, webp) attaches that file to the user message as
//! [`Content::Image`] (a `data:` URL the protocol drivers understand); the
//! extension only selects the token, the media type comes from the bytes. The
//! typed text is sent unchanged — `@` tokens stay in it, so the model can tie
//! each mention to its attached image. Paths with whitespace are not
//! supported; a `@token` with any other extension is ordinary text.
//!
//! Limits are measured on the **base64 payload actually uploaded**, not the
//! file on disk: base64 inflates by 4/3, and providers cap the encoded image
//! and the whole request. The per-image cap is the model's
//! (`max_image_base64_bytes`); the per-message budget below is myco's own and does
//! not vary by model. Per-message budgets alone cannot keep a session under
//! the request cap — images accumulate in history — so the drivers also
//! preflight the composed request (`generative_model::MAX_REQUEST_BYTES`).
//! myco does not re-encode images; a file over the limit is rejected with the
//! sizes named so the user can downscale it and resubmit.

use std::path::PathBuf;

use myco_core::image::{looks_like_image_path, mib, read_image_data_url};
use myco_models::Content;

/// Limit on the combined upload payload of one message's attachments. Well
/// under the request cap so a single message still leaves room for the
/// conversation it is appended to.
///
/// Deliberately not per-model: it bounds a *message*, not an image. A model
/// configured above this still gets its full `max_image_base64_bytes` through
/// `view_image` (one image, one tool result) — only batching many `@path`
/// mentions into a single message is held to this budget.
pub const MAX_MESSAGE_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Expand `@<path>` image mentions in `input` into content blocks for
/// `Agent::interact`: attached images first (providers prefer
/// image-before-text), then the input text exactly as typed. Repeated
/// mentions of one path attach it once. Any unreadable, oversized, or
/// non-image file fails the whole message so the user can fix it and resubmit —
/// nothing is silently dropped.
///
/// `max_image_base64_bytes` is the driving model's per-image cap.
pub fn expand_image_attachments(
    input: &str,
    max_image_base64_bytes: u64,
) -> Result<Vec<Content>, String> {
    let mut content = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut encoded_total: u64 = 0;

    for token in input.split_whitespace() {
        // Sentence position: "see @shot.png." / "(@shot.png)" — trailing
        // punctuation is not part of the path.
        let token =
            token.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        if !looks_like_image_path(path) {
            continue;
        }
        if seen.contains(&path) {
            continue;
        }

        let expanded = expand_home(path);
        // `read_image_data_url` caps each image; this budget caps the message,
        // measured on the same `data:` URL the request will carry.
        let source = read_image_data_url(&expanded, &format!("@{path}"), max_image_base64_bytes)?;
        encoded_total += source.len() as u64;
        if encoded_total > MAX_MESSAGE_ATTACHMENT_BYTES {
            return Err(format!(
                "attachments total {} encoded for upload; the per-message limit is \
                 {}. Send fewer images per message",
                mib(encoded_total),
                mib(MAX_MESSAGE_ATTACHMENT_BYTES),
            ));
        }
        content.push(Content::Image { source });
        seen.push(path);
    }

    content.push(Content::Text {
        text: input.to_string(),
    });
    Ok(content)
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use myco_config::DEFAULT_MAX_IMAGE_BASE64_BYTES;
    use myco_test_support::temp_dir;
    use std::fs;

    /// Real magic numbers: the media type is sniffed from these bytes.
    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];

    #[test]
    fn plain_text_passes_through() {
        let parsed =
            expand_image_attachments("just words, no files", DEFAULT_MAX_IMAGE_BASE64_BYTES)
                .unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(matches!(
            &parsed[0],
            Content::Text { text } if text == "just words, no files"
        ));
    }

    #[test]
    fn attaches_image_as_data_url_and_keeps_text_as_typed() {
        let dir = temp_dir("attach-png");
        let path = dir.path().join("shot.png");
        fs::write(&path, PNG).unwrap();
        let input = format!("what is wrong in @{}?", path.display());

        let parsed = expand_image_attachments(&input, DEFAULT_MAX_IMAGE_BASE64_BYTES).unwrap();
        assert_eq!(parsed.len(), 2);
        match &parsed[0] {
            Content::Image { source } => {
                let data = source.strip_prefix("data:image/png;base64,").unwrap();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                assert_eq!(decoded, PNG);
            }
            other => panic!("expected image first, got {other:?}"),
        }
        // Text is unchanged: the `@` mention stays for the model to reference.
        assert!(matches!(
            &parsed[1],
            Content::Text { text } if *text == input
        ));
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_path() {
        let dir = temp_dir("attach-punct");
        let path = dir.path().join("shot.png");
        fs::write(&path, PNG).unwrap();

        let parsed = expand_image_attachments(
            &format!("look at @{}.", path.display()),
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(matches!(&parsed[0], Content::Image { .. }));
    }

    /// The uppercase extension selects the token; the media type is sniffed.
    #[test]
    fn uppercase_extension_is_still_an_attachment_mention() {
        let dir = temp_dir("attach-jpg");
        let path = dir.path().join("photo.JPG");
        fs::write(&path, JPEG).unwrap();

        let parsed = expand_image_attachments(
            &format!("@{}", path.display()),
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap();
        match &parsed[0] {
            Content::Image { source } => {
                assert!(source.starts_with("data:image/jpeg;base64,"), "{source}");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn non_image_at_tokens_are_ordinary_text() {
        let parsed = expand_image_attachments(
            "ping @alice about @notes.txt",
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn missing_file_fails_the_message_naming_the_path() {
        let err = expand_image_attachments(
            "see @definitely-missing.png",
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap_err();
        assert!(err.contains("@definitely-missing.png"), "{err}");
    }

    /// Individually-legal images still fail as a batch: the per-message budget
    /// is what keeps one turn from blowing the request cap on its own.
    /// (The per-image cap itself lives in `core::image` and is tested there.)
    #[test]
    fn attachments_over_the_message_budget_fail() {
        let dir = temp_dir("attach-budget");
        // 3.5 MiB each → ~4.67 MiB encoded; five clear the 20 MiB budget.
        let mut bytes = vec![0u8; 7 * 512 * 1024];
        bytes[..PNG.len()].copy_from_slice(PNG);
        let mut input = String::new();
        for i in 0..5 {
            let path = dir.path().join(format!("shot{i}.png"));
            fs::write(&path, &bytes).unwrap();
            input.push_str(&format!("@{} ", path.display()));
        }

        let err = expand_image_attachments(&input, DEFAULT_MAX_IMAGE_BASE64_BYTES).unwrap_err();
        assert!(err.contains("per-message limit is 20.0 MiB"), "{err}");
    }

    /// The per-image cap is the model's: the same attachment that fails under
    /// a 1 MiB model attaches under an 8 MiB one.
    #[test]
    fn per_image_cap_comes_from_the_model() {
        let dir = temp_dir("attach-cap");
        let path = dir.path().join("shot.png");
        // 4 MiB file → ~5.3 MiB encoded.
        let mut bytes = vec![0u8; 4 * 1024 * 1024];
        bytes[..PNG.len()].copy_from_slice(PNG);
        fs::write(&path, &bytes).unwrap();
        let input = format!("@{}", path.display());

        let err = expand_image_attachments(&input, 1024 * 1024).unwrap_err();
        assert!(err.contains("the limit is 1.0 MiB"), "{err}");

        let parsed = expand_image_attachments(&input, 8 * 1024 * 1024).unwrap();
        assert!(matches!(&parsed[0], Content::Image { .. }));
    }

    /// An `@mention` that is not actually an image fails the message rather
    /// than sending bytes the provider will reject.
    #[test]
    fn image_extension_over_non_image_bytes_fails_the_message() {
        let dir = temp_dir("attach-fake");
        let path = dir.path().join("notes.png");
        fs::write(&path, b"just text").unwrap();

        let err = expand_image_attachments(
            &format!("@{}", path.display()),
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap_err();
        assert!(
            err.contains("is not a png, jpeg, gif, or webp image"),
            "{err}"
        );
    }

    #[test]
    fn repeated_mention_attaches_once() {
        let dir = temp_dir("attach-dup");
        let path = dir.path().join("shot.png");
        fs::write(&path, PNG).unwrap();
        let p = path.display();

        let parsed = expand_image_attachments(
            &format!("@{p} and again @{p}"),
            DEFAULT_MAX_IMAGE_BASE64_BYTES,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2); // one image + the text
    }
}
