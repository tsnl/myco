//! Host tool service: read an image file into the conversation.

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::Async;
use crate::core::image::{image_file_data_url, image_media_type};
use crate::generative_model::{self, Content, ToolResult};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Look at an image file: returns the image itself, so you can read screenshots, diagrams,
and rendered output instead of guessing from filenames.

Supported: png, jpg/jpeg, gif, webp — up to 5 MiB each. Anything else (including text
files) belongs in `str_replace_based_edit_tool` view, which this tool does not replace.
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
        let Some(media_type) = image_media_type(path) else {
            return Err(format!(
                "'{path}' is not a supported image (png, jpg, jpeg, gif, webp). \
                 Use str_replace_based_edit_tool view for text files."
            ));
        };

        // A directory can carry an image extension; reading it would surface a
        // confusing OS error instead of naming the real problem.
        let path_buf = PathBuf::from(path);
        let metadata =
            std::fs::metadata(&path_buf).map_err(|e| format!("cannot read image '{path}': {e}"))?;
        if !metadata.is_file() {
            return Err(format!("'{path}' is not a file"));
        }

        let source = image_file_data_url(&path_buf, media_type, &format!("'{path}'"))?;
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
    use serde_json::json;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!("myco-view-image-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

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

    fn error_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn returns_image_block_as_data_url() {
        let tmp = temp_dir();
        let path = tmp.0.join("shot.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(!result.is_error, "{}", error_text(&result));
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            // 0x89 P N G → iVBORw==
            Content::Image { source } => assert_eq!(source, "data:image/png;base64,iVBORw=="),
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_extension_maps_media_type() {
        let tmp = temp_dir();
        let path = tmp.0.join("photo.JPG");
        std::fs::write(&path, [1]).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(!result.is_error, "{}", error_text(&result));
        match &result.content[0] {
            Content::Image { source } => {
                assert!(source.starts_with("data:image/jpeg;base64,"), "{source}");
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn non_image_extension_points_at_the_editor() {
        let tmp = temp_dir();
        let path = tmp.0.join("notes.txt");
        std::fs::write(&path, "hi").unwrap();

        let result = view(&path.to_string_lossy());
        assert!(result.is_error);
        let text = error_text(&result);
        assert!(text.contains("not a supported image"), "{text}");
        assert!(text.contains("str_replace_based_edit_tool"), "{text}");
    }

    /// A directory named `*.png` must not be read as image bytes.
    #[test]
    fn directory_with_image_extension_errors() {
        let tmp = temp_dir();
        let dir = tmp.0.join("assets.png");
        std::fs::create_dir_all(&dir).unwrap();

        let result = view(&dir.to_string_lossy());
        assert!(result.is_error);
        assert!(error_text(&result).contains("is not a file"), "{result:?}");
    }

    #[test]
    fn missing_file_errors_naming_the_path() {
        let result = view("/definitely/missing/shot.png");
        assert!(result.is_error);
        assert!(
            error_text(&result).contains("/definitely/missing/shot.png"),
            "{result:?}"
        );
    }

    #[test]
    fn oversized_image_errors_with_limit() {
        let tmp = temp_dir();
        let path = tmp.0.join("big.png");
        std::fs::write(&path, vec![0u8; MAX_IMAGE_BYTES as usize + 1]).unwrap();

        let result = view(&path.to_string_lossy());
        assert!(result.is_error);
        assert!(error_text(&result).contains("limit is 5 MiB"), "{result:?}");
    }
}
