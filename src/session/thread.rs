//! Ordered conversation histories. Only the final thread accepts new context.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

use crate::core::uuid_simple_hex;
use crate::generative_model::{Message, TokenUsage};

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_id: Option<String>,
    pub messages: Vec<Message>,
    /// Acceptance times keyed by message index. Absence means unknown.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub user_turn_timestamps: BTreeMap<usize, DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<TokenUsage>,
    /// Intent saved before external work. It is reconciled before model input
    /// after restart; a pending tool batch must never be dispatched again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_operation: Option<crate::agent::PendingOperation>,
}

impl Thread {
    pub(super) fn new() -> Self {
        Self {
            id: uuid_simple_hex(uuid::Uuid::new_v4()),
            created_at: Utc::now(),
            predecessor_id: None,
            messages: Vec::new(),
            user_turn_timestamps: BTreeMap::new(),
            last_usage: None,
            pending_operation: None,
        }
    }
}

pub(super) fn deserialize_threads<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Thread>, D::Error> {
    let threads = Vec::<Thread>::deserialize(deserializer)?;
    validate_threads(&threads).map_err(serde::de::Error::custom)?;
    Ok(threads)
}

pub(super) fn validate_threads(threads: &[Thread]) -> Result<(), String> {
    if threads.is_empty() {
        return Err(String::from("session has no threads"));
    }
    let mut seen = HashSet::new();
    for thread in threads {
        // Historical observations remain readable even if malformed. Execution
        // validates full context; an explicit pending intent must be consistent.
        if thread.pending_operation.is_some() {
            crate::agent::validate_checkpoint(&thread.messages, thread.pending_operation)
                .map_err(|error| format!("thread {}: {error}", thread.id))?;
        }
        if thread.user_turn_timestamps.keys().any(|&index| {
            !thread
                .messages
                .get(index)
                .is_some_and(Message::is_user_turn)
        }) {
            return Err("user turn timestamps must refer to user messages".into());
        }
        if matches!(thread.id.as_str(), "" | "." | "..")
            || thread.id.contains(['/', '\\'])
            || seen.contains(&thread.id)
        {
            return Err(String::from(
                "thread ids must be unique and nonempty path components",
            ));
        }
        if thread
            .predecessor_id
            .as_ref()
            .is_some_and(|id| !seen.contains(id))
        {
            return Err(String::from(
                "thread predecessor must be an older thread in this session",
            ));
        }
        seen.insert(thread.id.clone());
    }
    Ok(())
}

pub(super) fn upgrade_v2(value: &mut serde_json::Value) -> Result<(), String> {
    let document = value.as_object_mut().ok_or("session must be an object")?;
    let messages = document
        .remove("messages")
        .ok_or("v2 session is missing messages")?;
    if document.contains_key("threads") {
        return Err("v2 session unexpectedly contains threads".into());
    }
    let id = document.get("id").ok_or("v2 session is missing id")?;
    let created_at = document
        .get("created_at")
        .ok_or("v2 session is missing created_at")?;
    let mut thread = serde_json::json!({"id": id, "created_at": created_at, "messages": messages});
    if let Some(usage) = document.remove("last_usage") {
        thread["last_usage"] = usage;
    }
    document.insert("threads".into(), serde_json::json!([thread]));
    document.insert(
        "version".into(),
        serde_json::json!(super::SESSION_FILE_VERSION),
    );
    Ok(())
}

/// Convert the runtime text shapes written by formats 2–4. Model-facing text
/// and message positions stay identical; genuine submissions keep their times.
pub(super) fn upgrade_system_parts(threads: &mut [Thread]) {
    use crate::generative_model::{Content, TurnEndReason};
    for thread in threads {
        let mut truncated = false;
        for (index, message) in thread.messages.iter_mut().enumerate() {
            if let Message::UserMessage { content } = message {
                let submitted = thread.user_turn_timestamps.contains_key(&index);
                for part in content {
                    let Content::Text { text } = part else {
                        continue;
                    };
                    let kind = if text.starts_with("# Session\n\n- Session id: `")
                        && text.contains("\n- Started: ")
                    {
                        "session"
                    } else if index == 0
                        && thread.predecessor_id.is_some()
                        && text.starts_with("# Compaction resume\n\n")
                    {
                        "compaction"
                    } else if !submitted
                        && (text == crate::prompts::COMPACTION_RESUMPTION
                            || (truncated && text == crate::agent::CONTINUE_PROMPT))
                    {
                        "continuation"
                    } else {
                        continue;
                    };
                    *part = Content::System {
                        kind: kind.into(),
                        text: text.clone(),
                        data: serde_json::json!({"legacy":true}),
                    };
                }
            }
            truncated = matches!(
                message,
                Message::AssistantMessage {
                    turn_end_reason: Some(TurnEndReason::MaxTokens),
                    ..
                }
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::session::Session;
    use crate::test_support::temp_dir;

    #[test]
    fn upgrading_runtime_text_preserves_model_text_and_real_submissions() {
        use crate::generative_model::{Content, Message};
        use crate::test_support::{assistant, user};
        let dir = temp_dir("hidden-upgrade");
        let path = dir.path().join("session.json");
        let mut session = Session::new("test");
        let stamp = crate::prompts::thread_stamp(
            &session.id,
            &session.active_thread().id,
            session.created_at,
        );
        session.active_thread_mut().messages = vec![
            Message::UserMessage {
                content: vec![
                    Content::Text {
                        text: stamp.clone(),
                    },
                    Content::Text {
                        text: "human task".into(),
                    },
                ],
            },
            assistant("done"),
            user(crate::prompts::COMPACTION_RESUMPTION),
            user(crate::prompts::COMPACTION_RESUMPTION),
        ];
        let accepted = chrono::Utc::now();
        session
            .active_thread_mut()
            .user_turn_timestamps
            .insert(3, accepted);
        let mut json = serde_json::to_value(session).unwrap();
        json["version"] = 4.into();
        let original = serde_json::to_vec(&json).unwrap();
        std::fs::write(&path, &original).unwrap();
        let upgraded = Session::load(&path).unwrap();
        let messages = &upgraded.active_thread().messages;
        assert!(matches!(&messages[0], Message::UserMessage { content }
            if matches!(&content[0], Content::System { text, kind, .. } if text == &stamp && kind == "session")));
        assert!(!messages[2].is_user_turn());
        assert!(messages[3].is_user_turn());
        assert_eq!(upgraded.active_thread().user_turn_timestamps[&3], accepted);
        assert_eq!(
            crate::session::first_user_text_from_messages(messages).as_deref(),
            Some("human task")
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let encoded = serde_json::to_vec(&upgraded).unwrap();
        let restored = Session::from_json(&encoded).unwrap();
        assert_eq!(serde_json::to_vec(&restored).unwrap(), encoded);
    }

    #[test]
    fn loading_v2_preserves_the_source_file_and_gives_the_initial_thread_a_stable_id() {
        let dir = temp_dir("thread-v2");
        let path = dir.path().join("session.json");
        let original = include_bytes!("../../tests/fixtures/session_v2_all_variants.json");
        std::fs::write(&path, original).unwrap();
        let first = Session::load(&path).unwrap();
        let second = Session::load(&path).unwrap();
        assert_eq!(first.active_thread().id, first.id);
        assert_eq!(first.active_thread().id, second.active_thread().id);
        assert_eq!(first.threads().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn loading_v3_leaves_unknown_turn_times_and_the_source_file_unchanged() {
        let dir = temp_dir("thread-v3");
        let path = dir.path().join("session.json");
        let mut document = serde_json::to_value(Session::new("test")).unwrap();
        document["version"] = serde_json::json!(3);
        let original = serde_json::to_vec(&document).unwrap();
        std::fs::write(&path, &original).unwrap();
        let loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.version, super::super::SESSION_FILE_VERSION);
        assert!(loaded.active_thread().user_turn_timestamps.is_empty());
        assert!(!loaded.archived);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn malformed_legacy_documents_return_errors() {
        for data in [
            br#"{"version":2,"messages":[]}"#.as_slice(),
            br#"{"version":2,"messages":[],"id":"s"}"#.as_slice(),
        ] {
            assert!(Session::from_json(data).is_err());
        }
    }

    #[test]
    fn threads_require_unique_ids_and_older_predecessors() {
        let session = Session::new("test");
        let base = serde_json::to_value(&session).unwrap();
        let first = base["threads"][0].clone();
        for threads in [
            serde_json::json!([]),
            serde_json::json!([first.clone(), first.clone()]),
            serde_json::json!([{"id":"future", "created_at":first["created_at"], "messages":[], "predecessor_id":first["id"]}, first]),
        ] {
            let mut invalid = base.clone();
            invalid["threads"] = threads;
            assert!(Session::from_json(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }
}
