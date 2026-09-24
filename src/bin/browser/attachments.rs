//! Browser uploads become verified image sidecars before entering the work queue.
//! Only references are retained in retries, queued messages, and saved history.

use base64::Engine as _;
use myco::core::image::{base64_len, image_data_url, mib};
use myco::core::image_store::ImageStore;
use myco::generative_model::Content;
use myco::session::{MAX_MESSAGE_ATTACHMENT_BYTES, expand_image_attachments};
use serde::Serialize;

//
// Upload policy
//

pub(super) const MAX_IMAGES: usize = 20;
pub(super) const ACTION_BODY_LIMIT: usize = MAX_MESSAGE_ATTACHMENT_BYTES as usize + 2 * 1024 * 1024;

#[derive(Clone, Serialize)]
pub(super) struct Limits {
    pub(super) max_image_base64_bytes: u64,
    max_message_attachment_bytes: u64,
    max_images: usize,
}

impl Limits {
    pub(super) fn new(max_image_base64_bytes: u64) -> Self {
        Self {
            max_image_base64_bytes,
            max_message_attachment_bytes: MAX_MESSAGE_ATTACHMENT_BYTES,
            max_images: MAX_IMAGES,
        }
    }
}

fn check_total(bytes: u64) -> Result<(), String> {
    if bytes > MAX_MESSAGE_ATTACHMENT_BYTES {
        return Err(format!(
            "attachments exceed the per-message limit of {}. Send fewer images per message",
            mib(MAX_MESSAGE_ATTACHMENT_BYTES),
        ));
    }
    Ok(())
}

fn decode(images: &[String]) -> Result<Vec<Content>, String> {
    if images.len() > MAX_IMAGES {
        return Err(format!("Attach at most {MAX_IMAGES} images per message."));
    }
    check_total(images.iter().map(|source| source.len() as u64).sum())?;
    images
        .iter()
        .enumerate()
        .map(|(index, source)| {
            if myco::core::image_store::is_reference(source) {
                // Queue edits reuse profile-local sidecars instead of uploading
                // the same images again. Delivery applies the current size caps.
                let store = ImageStore::for_profile()?;
                let path = store.path(source)?;
                std::fs::metadata(path)
                    .map_err(|error| format!("cannot read attachment: {error}"))?;
                return Ok(Content::Image {
                    source: source.clone(),
                });
            }
            let label = format!("attachment {}", index + 1);
            let data = source
                .strip_prefix("data:")
                .and_then(|data| data.split_once(";base64,"))
                .ok_or_else(|| format!("{label} must be a base64 image data URL"))?
                .1;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| format!("invalid {label}: {e}"))?;
            let source = image_data_url(&bytes, &label, MAX_MESSAGE_ATTACHMENT_BYTES)?;
            Ok(Content::Image { source })
        })
        .collect()
}

pub(super) fn externalize(images: &mut Vec<String>) -> Result<(), String> {
    if images.is_empty() {
        return Ok(());
    }
    let mut content = decode(images)?;
    ImageStore::for_profile()?.externalize(&mut content)?;
    *images = content
        .into_iter()
        .filter_map(|part| match part {
            Content::Image { source } => Some(source),
            _ => None,
        })
        .collect();
    Ok(())
}

//
// Message content
//

pub(super) fn content(
    text: &str,
    images: &[String],
    image_limit: u64,
) -> Result<Vec<Content>, String> {
    let mut expanded = expand_image_attachments(text, image_limit)?;
    if images.is_empty() {
        return Ok(expanded);
    }
    // Image-only messages do not need an empty text block in the provider request.
    expanded.retain(|part| !matches!(part, Content::Text { text } if text.trim().is_empty()));
    let store = ImageStore::for_profile()?;
    let mut total = expanded
        .iter()
        .filter_map(|part| match part {
            Content::Image { source } => Some(source.len() as u64),
            _ => None,
        })
        .sum::<u64>();
    for source in images {
        let path = store.path(source)?;
        let bytes = std::fs::metadata(&path)
            .map_err(|e| format!("cannot read attachment: {e}"))?
            .len();
        let encoded = base64_len(bytes);
        if encoded > image_limit {
            return Err(format!(
                "attachment is {} encoded for upload; the model's limit is {}. Resize or re-compress it and resubmit",
                mib(encoded),
                mib(image_limit)
            ));
        }
        let mime = source.rsplit(':').next().unwrap(); // Validated by ImageStore::path.
        total += encoded + format!("data:{mime};base64,").len() as u64;
        check_total(total)?;
    }
    Ok(images
        .iter()
        .cloned()
        .map(|source| Content::Image { source })
        .chain(expanded)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploaded_types_come_from_the_bytes_instead_of_the_browser_mime() {
        let images = vec!["data:image/jpeg;base64,iVBORw==".into()];
        assert!(
            matches!(&decode(&images).unwrap()[0], Content::Image { source } if source.starts_with("data:image/png;base64,"))
        );
    }

    #[test]
    fn malformed_or_unsupported_uploads_are_rejected_as_a_whole() {
        for source in [
            "https://example.com/image.png",
            "myco-image:sha256:bad:image/png",
            "data:image/png;base64,!",
            "data:image/png;base64,dGV4dA==",
        ] {
            assert!(decode(&["data:image/png;base64,iVBORw==".into(), source.into()]).is_err());
        }
        assert!(
            decode(&vec![
                "data:image/png;base64,iVBORw==".into();
                MAX_IMAGES + 1
            ])
            .is_err()
        );
        assert!(check_total(MAX_MESSAGE_ATTACHMENT_BYTES + 1).is_err());
    }
}
