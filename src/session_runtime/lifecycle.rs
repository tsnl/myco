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
        let text = super::description::describe(&record, !same_runtime);
        Some(Content::System {
            kind: "runtime".into(),
            text,
            data: serde_json::to_value(record).unwrap(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ToolResource;

    #[tokio::test(start_paused = true)]
    async fn an_aging_host_failure_does_not_add_repeated_runtime_records() {
        let harness = crate::harness::Harness::attach(crate::harness::HarnessConfig {
            remote_hosts: vec![crate::harness::HostConfig {
                name: "offline".into(),
                command: vec!["/missing/myco-host".into()],
            }],
            ..Default::default()
        })
        .await
        .unwrap();
        let owner = Uuid::new_v4();
        let result = harness
            .clone()
            .dispatch_tool_use(
                crate::generative_model::ToolUse {
                    name: "bash".into(),
                    input: serde_json::json!({"host":"offline", "command":"true"}),
                },
                owner,
                crate::core::CancelToken::new(),
            )
            .await;
        assert!(result.is_error);
        let model = ModelInfo::named("test");
        let first = RuntimeRecord::observe(
            &[],
            owner,
            model.clone(),
            harness.resources(owner).await,
            false,
        )
        .unwrap();
        let history = vec![Message::UserMessage {
            content: vec![first],
        }];
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert!(
            RuntimeRecord::observe(
                &history,
                owner,
                model,
                harness.resources(owner).await,
                false
            )
            .is_none()
        );
        assert!(
            harness
                .host_status()
                .iter()
                .find(|host| host.name == "offline")
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("10s ago")
        );
    }

    #[test]
    fn bounded_descriptions_preserve_complete_inventory_through_outage_and_restart() {
        let owner = Uuid::new_v4();
        let model = ModelInfo::named("model");
        let resources = vec![HostResources {
            host: "remote".into(),
            resources: Some(vec![ToolResource {
                tool: "bash".into(),
                id: "shell".into(),
                details: serde_json::json!({"command": "script".repeat(256 * 1024)}),
            }]),
            error: None,
        }];
        let first =
            RuntimeRecord::observe(&[], owner, model.clone(), resources.clone(), false).unwrap();
        let mut history = vec![Message::UserMessage {
            content: vec![first],
        }];
        assert_eq!(
            RuntimeRecord::latest(&history).unwrap().resources,
            resources
        );
        let missing = vec![HostResources {
            host: "remote".into(),
            resources: None,
            error: Some("offline".into()),
        }];
        let outage =
            RuntimeRecord::observe(&history, owner, model.clone(), missing.clone(), false).unwrap();
        history.push(Message::UserMessage {
            content: vec![outage],
        });
        let mut last_known = resources;
        last_known[0].error = Some("offline".into());
        assert_eq!(
            RuntimeRecord::latest(&history).unwrap().resources,
            last_known
        );
        let resumed =
            RuntimeRecord::observe(&history, Uuid::new_v4(), model, missing.clone(), false)
                .unwrap();
        history.push(Message::UserMessage {
            content: vec![resumed],
        });
        let record = RuntimeRecord::latest(&history).unwrap();
        assert_eq!(record.resources, missing);
        assert_eq!(record.unavailable_after_restart, last_known);
        for message in history {
            let Message::UserMessage { content } = message else {
                unreachable!()
            };
            let Content::System { text, .. } = &content[0] else {
                unreachable!()
            };
            assert!(text.len() <= 16 * 1024);
        }
    }

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
