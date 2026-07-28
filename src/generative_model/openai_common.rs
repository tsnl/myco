//! Settings and history conversion shared by the two OpenAI dialect drivers,
//! Responses and Chat Completions.
//!
//! Cross-*provider* scaffolding (client, SSE loop, slot remapping) lives in
//! [`super::driver_core`]; this is the layer above it that only the OpenAI
//! dialects share: identical backend settings, and the same rules for
//! rendering history text, image sources, and tool results.

use super::*;

/// Settings for either OpenAI dialect ([`BackendConfig::OpenAIResponses`] /
/// [`BackendConfig::OpenAICompletions`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpenAIBackendConfig {
    /// Base URL including any path prefix, e.g. `https://api.x.ai/v1` or
    /// `http://localhost:11434/v1`.
    pub base_url: String,
    pub auth_token: String,
    pub max_output_tokens: Option<usize>,
    pub debug_dump_api_requests: bool,
    /// When set, request provider reasoning at this effort.
    ///
    /// Sent as `reasoning.effort` (Responses) or `reasoning_effort` (Chat
    /// Completions). Servers that predate the field ignore it; OpenAI rejects
    /// it on non-reasoning models, so those need `thinking = "none"` in the
    /// catalog. Defaults to [`Effort::DEFAULT`] so reasoning is always requested.
    pub effort: Option<Effort>,
}

impl Default for OpenAIBackendConfig {
    fn default() -> Self {
        Self {
            // No built-in gateway: the catalog (config.toml) supplies base_url.
            base_url: String::new(),
            auth_token: String::new(),
            max_output_tokens: Some(8192),
            debug_dump_api_requests: false,
            effort: Some(Effort::DEFAULT),
        }
    }
}

/// History text: `Text` blocks joined by newlines.
///
/// Assistant turns carry no images; thinking is never echoed back to the
/// provider. User images and tool-result images take the image-part paths in
/// each dialect.
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

/// Wire value for `reasoning.effort` (Responses) / `reasoning_effort` (Chat
/// Completions), or `None` when the catalog opted out (`thinking = "none"`).
///
/// Unknown *fields* are typically ignored by OpenAI-compatible servers, but an
/// unknown *value* is a 400: `max` is Anthropic-only (both dialects take
/// minimal|low|medium|high), so clamp it.
pub(super) fn reasoning_effort(
    model: &ModelSpec,
    backend: &OpenAIBackendConfig,
) -> Option<&'static str> {
    if model.thinking != ThinkingMode::Effort {
        return None;
    }
    backend.effort.map(|effort| match effort {
        Effort::Max => Effort::High.as_str(),
        other => other.as_str(),
    })
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
