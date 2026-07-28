//! Conversion helpers shared by the two OpenAI-dialect drivers (Responses and
//! Chat Completions): both render history text the same way, accept the same
//! image source forms, and split a tool result into text plus images.

use super::{Content, ToolResult};

/// History text: `Text` blocks joined by newlines. Thinking is never echoed
/// back to the provider; images travel as their own content parts.
pub(super) fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            Content::Image { .. } | Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Image sources in arrival order (e.g. a `view_image` tool result).
pub(super) fn images_of(content: &[Content]) -> Vec<&str> {
    content
        .iter()
        .filter_map(|c| match c {
            Content::Image { source } => Some(source.as_str()),
            Content::Text { .. } | Content::Thinking { .. } => None,
        })
        .collect()
}

/// Image URL fields accept http(s) and `data:` URLs. Same source policy as the
/// Anthropic driver: pass URLs through, treat anything else as raw base64 PNG.
pub(super) fn image_url(source: &str) -> String {
    if source.starts_with("http://")
        || source.starts_with("https://")
        || source.starts_with("data:")
    {
        return source.to_string();
    }
    format!("data:image/png;base64,{source}")
}

/// The text half of a tool result, error-prefixed. Images are carried
/// separately by each dialect ([`images_of`]).
pub(super) fn tool_result_text(result: &ToolResult) -> String {
    let text = text_of(&result.content);
    if result.is_error && !text.is_empty() {
        format!("Error: {text}")
    } else if result.is_error {
        "Error".into()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_passes_urls_and_wraps_raw_base64() {
        assert_eq!(image_url("https://x.test/a.png"), "https://x.test/a.png");
        assert_eq!(
            image_url("data:image/jpeg;base64,AA"),
            "data:image/jpeg;base64,AA"
        );
        assert_eq!(image_url("iVBOR"), "data:image/png;base64,iVBOR");
    }

    #[test]
    fn text_and_images_split_by_kind() {
        let content = [
            Content::Thinking {
                text: "hidden".into(),
                signature: None,
                redacted: false,
            },
            Content::Text { text: "one".into() },
            Content::Image {
                source: "iVBOR".into(),
            },
            Content::Text { text: "two".into() },
        ];
        assert_eq!(text_of(&content), "one\ntwo");
        assert_eq!(images_of(&content), ["iVBOR"]);
    }

    #[test]
    fn tool_result_errors_are_prefixed() {
        assert_eq!(tool_result_text(&ToolResult::text("ok")), "ok");
        assert_eq!(tool_result_text(&ToolResult::err("boom")), "Error: boom");
        assert_eq!(tool_result_text(&ToolResult::err("")), "Error");
    }
}
