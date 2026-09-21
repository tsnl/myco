//! Serializable model identity and host-owned resources, without credentials.

use serde::{Deserialize, Serialize};

use crate::generative_model::{Effort, ModelSpec, Protocol};

pub(crate) fn latest_runtime_part(
    history: &[crate::generative_model::Message],
) -> Option<&crate::generative_model::Content> {
    use crate::generative_model::{Content, Message};
    history
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::UserMessage { content } => Some(content),
            _ => None,
        })
        .flat_map(|content| content.iter().rev())
        .find(|part| matches!(part, Content::System { kind, .. } if kind == "runtime"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub key: String,
    pub api_id: Option<String>,
    pub protocol: Option<Protocol>,
    pub effort: Option<Effort>,
}

impl ModelInfo {
    /// Caller-supplied models need only a stable label for comparisons and evals.
    pub fn named(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            api_id: None,
            protocol: None,
            effort: None,
        }
    }

    pub fn from_spec(spec: &ModelSpec, effort: Option<Effort>) -> Self {
        Self {
            key: spec.key.clone(),
            api_id: Some(spec.api_id.clone()),
            protocol: Some(spec.protocol),
            effort,
        }
    }
}

/// A retained tool handle. Details describe observations, not restored state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResource {
    pub tool: String,
    pub id: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResources {
    pub host: String,
    /// `None` means never observed. If `error` is present, these are only last
    /// known resources, not confirmation that they remain live.
    pub resources: Option<Vec<ToolResource>>,
    pub error: Option<String>,
}
