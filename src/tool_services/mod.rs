//! Host tool services (bash, text editor, manual, …).
//!
//! Registered on a [`crate::host::HostWorker`]. The **standard** catalog is the same on
//! every host. The agent **root** (in-process `local`) may instantiate additional
//! services at configuration time (e.g. `session_meta`) — still
//! [`ToolService`], just not installed on remotes.

use std::sync::Arc;

use crate::core::{Async, CancelToken};
use crate::generative_model;

pub mod bash_service;
pub use bash_service::BashService;

pub mod text_editor_service;
pub use text_editor_service::TextEditorService;

pub mod manual_service;
pub use manual_service::ManualService;

pub mod meta_tool_service;
pub use meta_tool_service::SessionMetaTool;

pub mod session_history_service;
pub use session_history_service::SessionHistoryTool;



pub mod list_recent_service;
pub use list_recent_service::ListRecentService;

/// Model-facing JSON Schema for a tool input type.
///
/// `schemars`' default output is hostile to OpenAI-compatible gateways and
/// weaker models: enum values hide behind `$defs`/`$ref` (template-based
/// stacks render properties without resolving refs, so the model never sees
/// the legal strings), `Option` fields carry `default: null` (inviting
/// all-keys-null fills), integers get non-standard `format: "uint*"` markers
/// (rejected by strict grammar compilers), and stray fields pass silently.
/// This generator inlines every subschema and scrubs the rest so each
/// property is self-describing on any stack:
///
/// - subschemas inlined: enums appear as `{"enum": [...], "type": "string"}`
///   in place, no `$defs` / `$ref` / `anyOf` null-arms;
/// - no `$schema` header, no root `title`;
/// - `default: null` removed everywhere (serde still treats omitted = null);
/// - schemars' numeric `format` markers (`uint`, `int32`, …) removed —
///   `minimum: 0` already carries the constraint;
/// - `additionalProperties: false` on the root object, so validating
///   gateways reject typo'd field names instead of silently dropping them.
///   (The harness-injected routing `host` is added to `properties`, so it
///   stays declared.)
pub fn tool_input_schema<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::generate::SchemaSettings::draft2020_12().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let mut value = settings.into_generator().root_schema_for::<T>().to_value();
    scrub_schema(&mut value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        if obj.get("type").and_then(|t| t.as_str()) == Some("object") {
            obj.entry("additionalProperties")
                .or_insert(serde_json::Value::Bool(false));
        }
    }
    value
}

/// Recursively drop `default: null` and schemars' non-standard numeric
/// `format` markers (see [`tool_input_schema`]).
fn scrub_schema(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if map.get("default").is_some_and(|d| d.is_null()) {
                map.remove("default");
            }
            if map.get("format").and_then(|f| f.as_str()).is_some_and(|f| {
                matches!(
                    f,
                    "uint"
                        | "uint8"
                        | "uint16"
                        | "uint32"
                        | "uint64"
                        | "uint128"
                        | "int"
                        | "int8"
                        | "int16"
                        | "int32"
                        | "int64"
                        | "int128"
                        | "float"
                        | "double"
                )
            }) {
                map.remove("format");
            }
            for (_, child) in map.iter_mut() {
                scrub_schema(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_schema(item);
            }
        }
        _ => {}
    }
}

/// Ambient context for host tool-service invocations.
#[derive(Clone)]
pub struct HostDispatchContext {
    /// Agent that owns this call; used for session ownership.
    pub agent_id: uuid::Uuid,
    /// Cancel signal for the in-flight call / agent turn.
    pub cancel: CancelToken,
}

impl HostDispatchContext {
    pub fn new(agent_id: uuid::Uuid, cancel: CancelToken) -> Self {
        Self { agent_id, cancel }
    }
}

/// Best-effort SIGKILL of a whole process group: `kill(2)` with `-pgid`.
///
/// Tool children are spawned with `.process_group(0)`, so the leader pid is
/// also the pgid; killing only the leader would leave grandchildren orphaned
/// under init. Direct syscall (no external `kill` binary), sync, and safe to
/// call from `Drop`. Errors (e.g. group already gone) are ignored.
pub(crate) fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    // SAFETY: kill(2) takes a pid and a signal number; no pointers involved.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// A placeable host tool capability.
pub trait ToolService: Send + Sync + 'static {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec>;

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult>;

    /// Called when an agent session ends so services can drop agent-scoped state
    /// (e.g. bash sessions owned by that agent). Default: no-op.
    fn on_agent_finished(&self, _agent_id: uuid::Uuid) {}

    /// One-line summaries of work this service still has running for
    /// `agent_id` (e.g. live bash sessions), for prompt-time display between
    /// turns. Must not block. Default: none.
    fn running_tool_summaries(&self, _agent_id: uuid::Uuid) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(schemars::JsonSchema, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Kind {
        Alpha,
        Beta,
    }

    #[derive(schemars::JsonSchema, serde::Deserialize)]
    #[allow(dead_code)]
    struct Probe {
        kind: Kind,
        #[serde(default)]
        maybe_kind: Option<Kind>,
        #[serde(default)]
        count: Option<usize>,
    }

    #[test]
    fn tool_input_schema_is_inline_and_scrubbed() {
        let schema = tool_input_schema::<Probe>();
        let text = schema.to_string();

        // Enums are inlined where they are used — nothing behind a ref.
        assert!(!text.contains("$defs"), "{text}");
        assert!(!text.contains("$ref"), "{text}");
        assert!(!text.contains("anyOf"), "{text}");
        assert!(!text.contains("$schema"), "{text}");
        assert_eq!(
            schema["properties"]["kind"]["enum"],
            serde_json::json!(["alpha", "beta"]),
            "{text}"
        );
        // Optional enum: legal strings still visible, null arm merged in.
        assert_eq!(
            schema["properties"]["maybe_kind"]["enum"],
            serde_json::json!(["alpha", "beta", null]),
            "{text}"
        );

        // No all-null bait, no non-standard numeric formats.
        assert!(!text.contains("\"default\":null"), "{text}");
        assert!(
            schema["properties"]["count"].get("format").is_none(),
            "{text}"
        );
        assert_eq!(schema["properties"]["count"]["minimum"], 0, "{text}");

        // Required + closed object.
        assert_eq!(schema["required"], serde_json::json!(["kind"]), "{text}");
        assert_eq!(
            schema["additionalProperties"],
            serde_json::json!(false),
            "{text}"
        );
        assert!(schema.get("title").is_none(), "{text}");
    }
}
