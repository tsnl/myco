//! Ordered conversation histories. Only the final thread accepts new context.

use std::collections::HashSet;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage: Option<TokenUsage>,
}

impl Thread {
    pub(super) fn new() -> Self {
        Self {
            id: uuid_simple_hex(uuid::Uuid::new_v4()),
            created_at: Utc::now(),
            predecessor_id: None,
            messages: Vec::new(),
            last_usage: None,
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

#[cfg(test)]
mod tests {
    use crate::session::Session;
    use crate::test_support::temp_dir;

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
