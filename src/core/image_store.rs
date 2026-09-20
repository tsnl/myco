//! Content-addressed image sidecars. Histories keep references; provider requests
//! resolve only the images in their input. Blobs precede references on disk and
//! are never garbage-collected while archived threads may still refer to them.

use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use ring::digest::{SHA256, digest};

use crate::core::{AsyncStream, atomically_write, myco_home};
use crate::generative_model::{
    Content, GenerateError, GenerationEvent, GenerationFailure, GenerativeModel, Message,
};

const PREFIX: &str = "myco-image:sha256:";
const MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Clone)]
pub struct ImageStore {
    root: PathBuf,
}

impl ImageStore {
    pub fn for_profile() -> Result<Self, String> {
        Ok(Self::new(myco_home()?.join("images")))
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self, reference: &str) -> Result<PathBuf, String> {
        let (hash, _) = parse_reference(reference)?;
        Ok(self.root.join(&hash[..2]).join(hash))
    }

    pub fn externalize(&self, content: &mut [Content]) -> Result<(), String> {
        for part in content {
            if let Content::Image { source } = part {
                if source.starts_with(PREFIX) {
                    parse_reference(source)?;
                } else if !source.starts_with("http://") && !source.starts_with("https://") {
                    *source = self.put(source)?;
                }
            }
        }
        Ok(())
    }

    pub fn externalize_messages(&self, messages: &mut [Message]) -> Result<(), String> {
        visit_content(messages, |content| self.externalize(content))
    }

    fn put(&self, source: &str) -> Result<String, String> {
        let (mime, data) = if let Some(data) = source.strip_prefix("data:") {
            data.split_once(";base64,")
                .ok_or("image data URL must contain a base64 payload")?
        } else {
            ("image/png", source)
        };
        if !MEDIA_TYPES.contains(&mime) {
            return Err(format!("unsupported image media type {mime:?}"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| format!("invalid image base64: {error}"))?;
        let hash = sha256(&bytes);
        let reference = format!("{PREFIX}{hash}:{mime}");
        let path = self.path(&reference)?;
        match std::fs::read(&path) {
            Ok(existing) if sha256(&existing) != hash => {
                return Err(format!("corrupt image sidecar {}", path.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(path.parent().unwrap())
                    .map_err(|error| format!("create image store: {error}"))?;
                atomically_write(&path, &bytes)
                    .map_err(|error| format!("write image {}: {error}", path.display()))?;
            }
            Err(error) => return Err(format!("read image {}: {error}", path.display())),
        }
        Ok(reference)
    }

    pub fn resolve(&self, source: &str) -> Result<String, String> {
        if !source.starts_with(PREFIX) {
            return Ok(source.into());
        }
        let (hash, mime) = parse_reference(source)?;
        let path = self.path(source)?;
        let bytes = std::fs::read(&path).map_err(|error| format!("read image sidecar {}: {error}; restore the profile's images directory from backup", path.display()))?;
        if sha256(&bytes) != hash {
            return Err(format!(
                "corrupt image sidecar {}; SHA-256 mismatch",
                path.display()
            ));
        }
        Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }

    /// Copy exactly the referenced blobs when exporting a private eval case.
    pub fn copy_reference(&self, reference: &str, destination: &Self) -> Result<(), String> {
        let source = self.resolve(reference)?;
        if is_reference(reference) {
            destination.put(&source)?;
        }
        Ok(())
    }

    pub fn resolve_messages(&self, messages: &mut [Message]) -> Result<(), String> {
        visit_content(messages, |content| {
            for part in content {
                if let Content::Image { source } = part
                    && is_reference(source)
                {
                    *source = self.resolve(source)?;
                }
            }
            Ok(())
        })
    }
}

pub fn is_reference(source: &str) -> bool {
    source.starts_with(PREFIX)
}

fn parse_reference(source: &str) -> Result<(&str, &str), String> {
    let (hash, mime) = source
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .ok_or("invalid image sidecar reference")?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !MEDIA_TYPES.contains(&mime)
    {
        return Err("invalid image sidecar hash or media type".into());
    }
    Ok((hash, mime))
}

pub fn sha256(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn visit_content(
    messages: &mut [Message],
    mut visit: impl FnMut(&mut [Content]) -> Result<(), String>,
) -> Result<(), String> {
    for message in messages {
        match message {
            Message::UserMessage { content } | Message::AssistantMessage { content, .. } => {
                visit(content)?
            }
            Message::ToolResults { tool_use_results } => {
                for result in tool_use_results {
                    visit(&mut result.content)?;
                }
            }
        }
    }
    Ok(())
}

/// Wrap an application model so sidecar references never reach provider APIs.
/// Captures the store root; changing environment variables cannot redirect a run.
pub fn with_images(inner: Arc<dyn GenerativeModel>, store: ImageStore) -> Arc<dyn GenerativeModel> {
    Arc::new(ImageModel { inner, store })
}

struct ImageModel {
    inner: Arc<dyn GenerativeModel>,
    store: ImageStore,
}

impl GenerativeModel for ImageModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        if !input.iter().flat_map(Message::content).any(|part| {
            matches!(part,
            Content::Image { source } if is_reference(source))
        }) {
            return self.inner.generate(input);
        }
        let mut input = input.to_vec();
        if let Err(error) = self.store.resolve_messages(&mut input) {
            return Box::pin(futures::stream::once(async move {
                GenerationEvent::Failure(GenerationFailure::terminal(
                    GenerateError::ExecutionError(error),
                ))
            }));
        }
        self.inner.generate(&input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScriptedModel, assistant, temp_dir, temp_home, user};
    use crate::{ActiveSession, Session};
    use futures::StreamExt;

    fn inline(bytes: &[u8]) -> Content {
        Content::Image {
            source: format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
        }
    }

    #[test]
    fn duplicate_images_share_one_verified_blob_and_exports_are_independent() {
        let temp = temp_dir("sidecars");
        let store = ImageStore::new(temp.path().join("images"));
        let mut parts = vec![inline(b"image bytes"); 2];
        store.externalize(&mut parts).unwrap();
        assert_eq!(parts[0], parts[1]);
        let Content::Image { source } = &parts[0] else {
            panic!()
        };
        let path = store.path(source).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"image bytes");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        store.externalize(&mut [inline(b"image bytes")]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let export = ImageStore::new(temp.path().join("export"));
        store.copy_reference(source, &export).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(store.resolve(source).unwrap_err().contains("restore"));
        let restored = export.resolve(source).unwrap();
        assert_eq!(Content::Image { source: restored }, inline(b"image bytes"));
        std::fs::write(export.path(source).unwrap(), "changed bytes").unwrap();
        assert!(
            export
                .resolve(source)
                .unwrap_err()
                .contains("SHA-256 mismatch")
        );
        for bad in [
            format!("{PREFIX}../../secret:image/png"),
            format!("{PREFIX}{}:text/plain", "a".repeat(64)),
        ] {
            assert!(store.path(&bad).is_err());
        }
    }

    #[test]
    fn legacy_inline_history_is_externalized_once_and_compaction_does_not_read_archived_blobs() {
        let _home = temp_home("sidecar-session");
        let mut session = Session::new("test");
        let mut messages = vec![
            Message::UserMessage {
                content: vec![inline(&vec![42; 2 * 1024 * 1024])],
            },
            assistant("image observed"),
        ];
        for _ in 0..3 {
            messages.extend([user("continue"), assistant("working")]);
        }
        session.replace_context(messages, None);
        let mut legacy = serde_json::to_value(&session).unwrap();
        legacy["version"] = 5.into();
        let loaded = Session::from_json(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            loaded.active_thread().messages[0].content().any(
                |part| matches!(part, Content::Image { source } if source.starts_with("data:"))
            )
        );
        loaded.save().unwrap();
        let mut saved = Session::load(&session.json_path()).unwrap();
        let reference = saved.active_thread().messages[0]
            .content()
            .find_map(|part| match part {
                Content::Image { source } => Some(source.clone()),
                _ => None,
            })
            .unwrap();
        assert!(std::fs::metadata(saved.json_path()).unwrap().len() < 4000);
        let blob = ImageStore::for_profile().unwrap().path(&reference).unwrap();
        std::fs::remove_file(blob).unwrap();
        // The old image is outside the retained tail. Browsing/checkpointing and
        // compaction must not need its bytes to preserve the archived observation.
        let (successor, _) = crate::compact_thread(&saved, "Continue the task").unwrap();
        let active = ActiveSession::new(saved.clone());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let writer = active.writer().await;
            writer.commit_thread(successor).unwrap();
        });
        saved = Session::load(&saved.json_path()).unwrap();
        assert_eq!(saved.threads().len(), 2);
        assert!(std::fs::metadata(saved.json_path()).unwrap().len() < 7000);
        ImageStore::for_profile()
            .unwrap()
            .resolve_messages(&mut saved.active_thread_mut().messages)
            .unwrap();
        assert_eq!(saved.version, 6);
    }

    #[tokio::test]
    async fn missing_active_image_stops_before_provider_dispatch() {
        let temp = temp_dir("sidecar-missing");
        let store = ImageStore::new(temp.path().join("images"));
        let mut input = vec![Message::UserMessage {
            content: vec![inline(b"data")],
        }];
        store.externalize_messages(&mut input).unwrap();
        let source = input[0]
            .content()
            .find_map(|part| match part {
                Content::Image { source } => Some(source),
                _ => None,
            })
            .unwrap();
        std::fs::remove_file(store.path(source).unwrap()).unwrap();
        let inner = ScriptedModel::new(vec![]);
        let model = with_images(inner, store);
        assert!(
            matches!(model.generate(&input).next().await, Some(GenerationEvent::Failure(failure)) if failure.cause.to_string().contains("sidecar"))
        );
    }
}
