//! Host tool service: read an image file into the conversation.

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::Async;
use crate::core::image::read_image_data_url;
use crate::generative_model::{self, Content, ToolResult};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Look at an image file: returns the image itself, so you can read screenshots, diagrams,
and rendered output instead of guessing from filenames.

Supported: png, jpeg, gif, webp — up to 5 MiB each. The format is detected from the
file's contents, so the extension can be wrong or missing. Anything else (including
text files) belongs in `str_replace_based_edit_tool` view, which this does not replace.
"#;

/// Reads image files as [`Content::Image`]. Implements [`ToolService`] (host-placed).
#[derive(Default)]
pub struct ViewImageService;

impl ViewImageService {
    pub fn new() -> Self {
        Self
    }

    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "view_image".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
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

        let source = read_image_data_url(&path_buf, &format!("'{path}'"))?;
        Ok(Content::Image { source })
    }
}

impl ToolService for ViewImageService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs()
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
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
    use crate::core::CancelToken;
    use crate::core::image::MAX_IMAGE_BYTES;
    use crate::generative_model::ToolUse;
    use crate::test_support::{result_text, temp_dir};
    use serde_json::json;

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];

    fn view(path: &str) -> ToolResult {
        futures::executor::block_on(Arc::new(ViewImageService::new()).dispatch_tool_use(
            ToolUse {
                id: "t1".into(),
                name: "view_image".into(),
                input: json!({ "path": path }),
            },
            HostDispatchContext {
                agent_id: uuid::Uuid::nil(),
                cancel: CancelToken::new(),
            },
        ))
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
        std::fs::write(&path, vec![0u8; MAX_IMAGE_BYTES as usize + 1]).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("limit is 5 MiB"),
            "{result:?}"
        );
    }
}
