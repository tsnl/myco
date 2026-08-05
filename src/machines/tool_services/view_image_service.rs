//! Host tool service: read an image file into the conversation.

use crate::models as generative_model;
use std::path::PathBuf;
use std::sync::Arc;

use crate::core::Async;
use crate::core::image::{mib, read_image_data_url};
use myco_api::{Content, ToolResult};

use super::{HostDispatchContext, ToolService};

/// `{limit}` is the driving model's image cap — quoted so the model can skip a
/// call it would only fail.
const TOOL_DESCRIPTION: &str = r#"
Look at an image file: returns the image itself, so you can read screenshots, diagrams,
and rendered output instead of guessing from filenames.

Supported: png, jpeg, gif, webp — up to {limit} each (measured on the base64 payload,
which is 4/3 of the file). The format is detected from the file's contents, so the
extension can be wrong or missing. Anything else (including text files) belongs in
`str_replace_based_edit_tool` view, which this does not replace.
"#;

/// Reads image files as [`Content::Image`]. Implements [`ToolService`] (host-placed).
pub struct ViewImageService {
    /// Largest image this host will return, from the driving model's
    /// `max_image_base64_bytes` (see [`crate::machines::host::HostWorker::standard`]).
    max_image_base64_bytes: u64,
}

impl ViewImageService {
    pub fn new(max_image_base64_bytes: u64) -> Self {
        Self {
            max_image_base64_bytes,
        }
    }

    /// Tool schemas served by a service built with `max_image_base64_bytes` (no
    /// instance required; the cap is the only thing that varies).
    pub fn specs(max_image_base64_bytes: u64) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "view_image".to_string(),
            description: TOOL_DESCRIPTION.replace("{limit}", &mib(max_image_base64_bytes)),
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }

    fn execute(&self, input: Input) -> Result<Content, String> {
        let path = input.path.trim();
        if path.is_empty() {
            return Err("view_image requires a non-empty path".to_string());
        }

        // Reading a directory would surface a confusing OS error instead of
        // naming the real problem.
        let path_buf = PathBuf::from(path);
        let metadata =
            std::fs::metadata(&path_buf).map_err(|e| format!("cannot read image '{path}': {e}"))?;
        if !metadata.is_file() {
            return Err(format!("'{path}' is not a file"));
        }

        let source =
            read_image_data_url(&path_buf, &format!("'{path}'"), self.max_image_base64_bytes)?;
        Ok(Content::Image { source })
    }
}

impl ToolService for ViewImageService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs(self.max_image_base64_bytes)
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: myco_api::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<myco_api::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("invalid view_image input: {e}")),
            };
            match self.execute(input) {
                Ok(image) => ToolResult::ok(vec![image]),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Path to the image file to look at.
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_IMAGE_BASE64_BYTES;
    use crate::core::CancelToken;
    use crate::test_support::{result_text, temp_dir};
    use myco_api::ToolUse;
    use serde_json::json;

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];

    fn view(path: &str) -> ToolResult {
        view_capped(path, DEFAULT_MAX_IMAGE_BASE64_BYTES)
    }

    fn view_capped(path: &str, max_image_base64_bytes: u64) -> ToolResult {
        futures::executor::block_on(
            Arc::new(ViewImageService::new(max_image_base64_bytes)).dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "view_image".into(),
                    input: json!({ "path": path }),
                },
                HostDispatchContext {
                    agent_id: uuid::Uuid::nil(),
                    cancel: CancelToken::new(),
                },
            ),
        )
    }

    #[test]
    fn returns_image_block_as_data_url() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("shot.png");
        std::fs::write(&path, PNG).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(!result.is_error, "{}", result_text(&result));
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            // 0x89 P N G → iVBORw==
            Content::Image { source } => assert_eq!(source, "data:image/png;base64,iVBORw=="),
            other => panic!("expected image block, got {other:?}"),
        }
    }

    /// Screenshots are routinely saved without an extension; the format comes
    /// from the bytes, so there is nothing to gate on.
    #[test]
    fn reads_an_extensionless_file() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("clipboard-grab");
        std::fs::write(&path, JPEG).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(!result.is_error, "{}", result_text(&result));
        match &result.content[0] {
            Content::Image { source } => {
                assert!(source.starts_with("data:image/jpeg;base64,"), "{source}");
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    /// A wrong extension must not decide the media type on the wire.
    #[test]
    fn mislabeled_extension_uses_the_real_media_type() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("actually-a-jpeg.png");
        std::fs::write(&path, JPEG).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(!result.is_error, "{}", result_text(&result));
        match &result.content[0] {
            Content::Image { source } => {
                assert!(source.starts_with("data:image/jpeg;base64,"), "{source}");
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn text_file_is_rejected() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, "hi").unwrap();

        let result = view(&path.to_string_lossy());
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("is not a png, jpeg, gif, or webp image"),
            "{result:?}"
        );
    }

    /// A directory named `*.png` must not be read as image bytes.
    #[test]
    fn directory_with_image_extension_errors() {
        let tmp = temp_dir("view-image");
        let dir = tmp.path().join("assets.png");
        std::fs::create_dir_all(&dir).unwrap();

        let result = view(&dir.to_string_lossy());
        assert!(result.is_error);
        assert!(result_text(&result).contains("is not a file"), "{result:?}");
    }

    #[test]
    fn missing_file_errors_naming_the_path() {
        let result = view("/definitely/missing/shot.png");
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("/definitely/missing/shot.png"),
            "{result:?}"
        );
    }

    #[test]
    fn oversized_image_errors_with_limit() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("big.png");
        std::fs::write(
            &path,
            vec![0u8; DEFAULT_MAX_IMAGE_BASE64_BYTES as usize + 1],
        )
        .unwrap();

        let result = view(&path.to_string_lossy());
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("limit is 5.0 MiB"),
            "{result:?}"
        );
    }

    /// The cap is the configured model's, and crossing it fails *this tool
    /// use* — an error the model can act on, not a killed turn. The same file
    /// is served under a model configured for larger images.
    #[test]
    fn cap_is_per_model_and_only_fails_the_tool_use() {
        let tmp = temp_dir("view-image");
        let path = tmp.path().join("shot.png");
        // 4 MiB file → ~5.3 MiB encoded.
        let mut bytes = vec![0u8; 4 * 1024 * 1024];
        bytes[..PNG.len()].copy_from_slice(PNG);
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_string_lossy().to_string();

        let rejected = view_capped(&path, 2 * 1024 * 1024);
        assert!(rejected.is_error);
        assert!(
            result_text(&rejected).contains("limit is 2.0 MiB"),
            "{rejected:?}"
        );

        let accepted = view_capped(&path, 8 * 1024 * 1024);
        assert!(!accepted.is_error, "{}", result_text(&accepted));
        assert!(matches!(&accepted.content[0], Content::Image { .. }));
    }

    /// The advertised limit tracks the configuration; a stale "5 MiB" in the
    /// description would send the model after calls that cannot succeed.
    #[test]
    fn description_quotes_the_configured_limit() {
        let specs = ViewImageService::specs(20 * 1024 * 1024);
        assert!(specs[0].description.contains("20.0 MiB"), "{specs:?}");
    }
}
