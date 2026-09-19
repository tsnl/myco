//! Runtime observations are facts about one tool owner, never a restoration plan.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::{HostResources, ModelInfo};
use crate::generative_model::{Content, Message};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub runtime_id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub resumed: bool,
    pub model: ModelInfo,
    pub previous_model: Option<ModelInfo>,
    pub resources: Vec<HostResources>,
    pub unavailable_after_restart: Vec<HostResources>,
}

impl RuntimeRecord {
    pub fn latest(history: &[Message]) -> Option<Self> {
        match crate::core::latest_runtime_part(history)? {
            Content::System { data, .. } => serde_json::from_value(data.clone()).ok(),
            _ => unreachable!(),
        }
    }

    pub(super) fn observe(
        history: &[Message],
        runtime_id: Uuid,
        model: ModelInfo,
        mut resources: Vec<HostResources>,
        resume: bool,
    ) -> Option<Content> {
        let previous = Self::latest(history);
        let same_runtime = previous
            .as_ref()
            .is_some_and(|old| old.runtime_id == runtime_id);
        if same_runtime {
            for host in &mut resources {
                if host.resources.is_none() {
                    host.resources = previous
                        .as_ref()
                        .unwrap()
                        .resources
                        .iter()
                        .find(|old| old.host == host.host)
                        .and_then(|old| old.resources.clone());
                }
            }
        }
        if !resume
            && same_runtime
            && previous
                .as_ref()
                .is_some_and(|old| old.model == model && old.resources == resources)
        {
            return None;
        }
        let unavailable_after_restart = match &previous {
            Some(old) if same_runtime => old.unavailable_after_restart.clone(),
            Some(old) => old
                .resources
                .iter()
                .filter(|host| {
                    host.resources
                        .as_ref()
                        .is_some_and(|resources| !resources.is_empty())
                })
                .cloned()
                .collect(),
            None => vec![],
        };
        let record = Self {
            runtime_id,
            observed_at: Utc::now(),
            resumed: !history.is_empty() && (resume || !same_runtime),
            previous_model: previous
                .filter(|old| old.model != model)
                .map(|old| old.model),
            model,
            resources,
            unavailable_after_restart,
        };
        let mut text = format!(
            "Runtime {} at {}. {}\nModel: {}.",
            record.runtime_id,
            record.observed_at.to_rfc3339(),
            if record.resumed {
                "Session resumed; use the current runtime observations below."
            } else {
                "Current runtime observations."
            },
            serde_json::to_string(&record.model).unwrap()
        );
        if let Some(previous) = &record.previous_model {
            text.push_str(&format!(
                "\nModel or effort changed from {}.",
                serde_json::to_string(previous).unwrap()
            ));
        }
        if !same_runtime && record.resumed {
            text.push_str("\nThis is a new runtime. Saved conversation does not restore bash handles, captured output, or editor read fingerprints. Re-read files before editing. External side effects and background processes may still exist; verify before repeating work.");
        }
        describe_resources(&mut text, "Current tool resources", &record.resources);
        if !record.unavailable_after_restart.is_empty() {
            describe_resources(
                &mut text,
                "Handles from the previous runtime are unavailable here (last recorded inventory)",
                &record.unavailable_after_restart,
            );
        }
        Some(Content::System {
            kind: "runtime".into(),
            text,
            data: serde_json::to_value(record).unwrap(),
        })
    }
}

fn describe_resources(text: &mut String, label: &str, hosts: &[HostResources]) {
    text.push_str(&format!("\n{label}:"));
    for host in hosts {
        text.push_str(&format!("\n- {}: ", host.host));
        if let Some(error) = &host.error {
            text.push_str(&format!(
                "{error}; any listed resources are last known, not confirmed live. "
            ));
        }
        let Some(resources) = &host.resources else {
            continue;
        };
        if resources.is_empty() {
            text.push_str("no retained resources");
        }
        let editors = resources
            .iter()
            .filter(|resource| resource.tool == "str_replace_based_edit_tool")
            .count();
        if editors > 0 {
            text.push_str(&format!("{editors} editor read fingerprints; "));
        }
        for resource in resources
            .iter()
            .filter(|resource| resource.tool != "str_replace_based_edit_tool")
        {
            text.push_str(&format!(
                "{} handle {:?} {}; ",
                resource.tool, resource.id, resource.details
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ToolResource;

    #[test]
    fn unavailable_inventory_preserves_last_known_handles_and_restart_never_claims_them() {
        let owner = Uuid::new_v4();
        let model = ModelInfo::named("model");
        let handles = vec![ToolResource {
            tool: "bash".into(),
            id: "remote-shell".into(),
            details: serde_json::json!({}),
        }];
        let first = RuntimeRecord::observe(
            &[],
            owner,
            model.clone(),
            vec![HostResources {
                host: "remote".into(),
                resources: Some(handles.clone()),
                error: None,
            }],
            false,
        )
        .unwrap();
        let mut history = vec![Message::UserMessage {
            content: vec![first],
        }];
        let missing = vec![HostResources {
            host: "remote".into(),
            resources: None,
            error: Some("connection lost".into()),
        }];
        let unknown =
            RuntimeRecord::observe(&history, owner, model.clone(), missing.clone(), false).unwrap();
        history.push(Message::UserMessage {
            content: vec![unknown],
        });
        let record = RuntimeRecord::latest(&history).unwrap();
        assert_eq!(record.resources[0].resources.as_ref().unwrap(), &handles);
        assert!(record.resources[0].error.is_some());
        assert!(
            RuntimeRecord::observe(&history, owner, model.clone(), missing.clone(), false)
                .is_none()
        );
        let resumed =
            RuntimeRecord::observe(&history, Uuid::new_v4(), model, missing, false).unwrap();
        history.push(Message::UserMessage {
            content: vec![resumed],
        });
        let record = RuntimeRecord::latest(&history).unwrap();
        assert!(record.resources[0].resources.is_none());
        assert_eq!(
            record.unavailable_after_restart[0]
                .resources
                .as_ref()
                .unwrap(),
            &handles
        );
    }
}
