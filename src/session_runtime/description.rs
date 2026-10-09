//! A bounded model-facing projection. The complete observation stays in metadata.

use crate::core::{HostResources, ModelInfo, ToolResource};

use super::RuntimeRecord;

const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
const MAX_PREVIEW_BYTES: usize = 512;
const FOOTER_RESERVE: usize = 512;

//
// Runtime descriptions
//

pub(super) fn describe(record: &RuntimeRecord, new_runtime: bool) -> String {
    let mut output = Description::default();
    output.push(&format!(
        "Runtime {} at {}. {}\n",
        record.runtime_id,
        record.observed_at.to_rfc3339(),
        if record.resumed {
            "Session resumed; use the current runtime observations below."
        } else {
            "Current runtime observations."
        }
    ));
    if new_runtime && record.resumed {
        output.push("This is a new runtime. Saved conversation does not restore bash handles, captured output, or editor read fingerprints. Re-read files before editing. External side effects and background processes may still exist; verify before repeating work.\n");
    }
    output.model("Model", &record.model);
    if let Some(previous) = &record.previous_model {
        output.model("Model or effort changed from", previous);
    }
    output.resources("Current tool resources", &record.resources);
    if !record.unavailable_after_restart.is_empty() {
        output.resources(
            "Handles from the previous runtime are unavailable here (last recorded inventory)",
            &record.unavailable_after_restart,
        );
    }
    output.finish()
}

#[derive(Default)]
struct Description {
    text: String,
    omitted_hosts: usize,
    omitted_handles: usize,
    omitted_models: usize,
}

impl Description {
    /// Append complete rows, never partial identifiers or observation qualifiers.
    fn push(&mut self, text: &str) -> bool {
        if text.len() > (MAX_DESCRIPTION_BYTES - FOOTER_RESERVE).saturating_sub(self.text.len()) {
            return false;
        }
        self.text.push_str(text);
        true
    }

    fn model(&mut self, label: &str, model: &ModelInfo) {
        let size = model
            .key
            .len()
            .saturating_add(model.api_id.as_ref().map_or(0, String::len));
        if size > MAX_PREVIEW_BYTES
            || !self.push(&format!(
                "{label}: {}.\n",
                serde_json::to_string(model).unwrap()
            ))
        {
            self.omitted_models += 1;
        }
    }

    fn resources(&mut self, label: &str, hosts: &[HostResources]) {
        if !self.push(&format!("{label}:\n")) {
            self.omit_hosts(hosts);
            return;
        }
        for host in hosts {
            self.host(host);
        }
    }

    fn omit_hosts(&mut self, hosts: &[HostResources]) {
        self.omitted_hosts += hosts.len();
        self.omitted_handles += hosts.iter().map(handle_count).sum::<usize>();
    }

    fn host(&mut self, host: &HostResources) {
        if host.host.len() > MAX_PREVIEW_BYTES {
            self.omit_hosts(std::slice::from_ref(host));
            return;
        }
        let mut row = format!("- host={:?}: ", host.host);
        if host.error.is_some() && host.resources.is_some() {
            row.push_str("last-known inventory: ");
        }
        match &host.resources {
            Some(resources) if resources.is_empty() => row.push_str("no retained resources"),
            Some(resources) => {
                let editors = resources
                    .iter()
                    .filter(|resource| is_editor(resource))
                    .count();
                row.push_str(&format!(
                    "{} handles; {editors} editor read fingerprints",
                    resources.len() - editors
                ));
            }
            None => row.push_str("inventory unavailable"),
        }
        if let Some(error) = &host.error {
            row.push_str(&format!(
                "; {}; listed resources are last known, not confirmed live",
                preview(error)
            ));
        }
        row.push('\n');
        if !self.push(&row) {
            self.omit_hosts(std::slice::from_ref(host));
            return;
        }
        for resource in host
            .resources
            .iter()
            .flatten()
            .filter(|resource| !is_editor(resource))
        {
            match resource_row(&host.host, resource) {
                Some(row) if self.push(&row) => {}
                _ => self.omitted_handles += 1,
            }
        }
    }

    fn finish(mut self) -> String {
        if self.omitted_hosts + self.omitted_handles + self.omitted_models > 0 {
            self.text.push_str(&format!(
                "Description limit: {} hosts, {} handles, and {} model identities omitted; omission does not mean resources are absent. Use bash action=list on the configured hosts for current handles, session_history for prior observations, and session metadata for model identity.\n",
                self.omitted_hosts, self.omitted_handles, self.omitted_models
            ));
        }
        debug_assert!(self.text.len() <= MAX_DESCRIPTION_BYTES);
        self.text
    }
}

fn is_editor(resource: &ToolResource) -> bool {
    resource.tool == "str_replace_based_edit_tool"
}

fn handle_count(host: &HostResources) -> usize {
    host.resources
        .iter()
        .flatten()
        .filter(|resource| !is_editor(resource))
        .count()
}

//
// Resource fields
//

fn resource_row(host: &str, resource: &ToolResource) -> Option<String> {
    let instance = resource.details["instance_id"].as_str();
    let identity_bytes = resource
        .id
        .len()
        .saturating_add(resource.tool.len())
        .saturating_add(instance.map_or(0, str::len));
    // Keep identifiers whole. Oversized rows are discoverable through the
    // explicit omission count and the owning tool's inventory operation.
    if identity_bytes > MAX_DESCRIPTION_BYTES - FOOTER_RESERVE {
        return None;
    }
    let mut row = format!(
        "  host={host:?} tool={:?} handle={:?}",
        resource.tool, resource.id
    );
    if let Some(instance) = instance {
        row.push_str(&format!(" instance_id={instance:?}"));
    }
    if resource.tool == "bash" {
        bash_fields(&mut row, resource);
    } else {
        row.push_str("; inspect the owning tool for details");
    }
    row.push('\n');
    Some(row)
}

fn bash_fields(row: &mut String, resource: &ToolResource) {
    let details = &resource.details;
    if let Some(pid) = details["pid"].as_u64() {
        row.push_str(&format!(" pid={pid}"));
    }
    for field in ["process_exited", "output_closed"] {
        if let Some(value) = details[field].as_bool() {
            row.push_str(&format!(" {field}={value}"));
        }
    }
    for field in ["exit_code", "exit_signal"] {
        if let Some(value) = details[field].as_i64() {
            row.push_str(&format!(" {field}={value}"));
        }
    }
    if let Some(command) = details["command"].as_str() {
        row.push_str(&format!(" command={}", preview(command)));
    }
}

/// Quote control characters so arbitrary output cannot manufacture new rows.
/// Bound the encoded preview, including the notice and UTF-8 characters.
fn preview(value: &str) -> String {
    let mut end = value.len().min(MAX_PREVIEW_BYTES);
    loop {
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let suffix = if end == value.len() {
            String::new()
        } else {
            format!("… [{} bytes omitted]", value.len() - end)
        };
        let quoted = serde_json::to_string(&format!("{}{suffix}", &value[..end])).unwrap();
        if quoted.len() <= MAX_PREVIEW_BYTES {
            return quoted;
        }
        end = end.saturating_sub((quoted.len() - MAX_PREVIEW_BYTES).max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    fn record(resources: Vec<HostResources>) -> RuntimeRecord {
        RuntimeRecord {
            runtime_id: Uuid::new_v4(),
            observed_at: Utc::now(),
            resumed: false,
            model: ModelInfo::named("model"),
            previous_model: None,
            resources,
            unavailable_after_restart: vec![],
        }
    }

    fn host(resources: Vec<ToolResource>) -> HostResources {
        HostResources {
            host: "remote".into(),
            resources: Some(resources),
            error: None,
        }
    }

    fn shell(id: impl Into<String>, command: impl Into<String>) -> ToolResource {
        ToolResource {
            tool: "bash".into(),
            id: id.into(),
            details: json!({"command": command.into(), "instance_id": "unique-instance",
                "pid": 1234, "process_exited": false, "output_closed": false}),
        }
    }

    #[test]
    fn previews_bound_encoded_unicode_and_control_characters() {
        for input in ["small".into(), "🦀".repeat(1000), "\0\n\r\"\\".repeat(1000)] {
            let text = preview(&input);
            assert!(text.len() <= MAX_PREVIEW_BYTES);
            assert!(!text.contains('\n'));
            let decoded: String = serde_json::from_str(&text).unwrap();
            if input.len() > MAX_PREVIEW_BYTES {
                assert!(decoded.contains("bytes omitted]"));
            } else {
                assert_eq!(decoded, input);
            }
        }
    }

    #[test]
    fn arbitrary_details_are_bounded_without_changing_actionable_identity() {
        let mut resource = shell("shell\n\"handle", "🦀\n".repeat(256 * 1024));
        resource.details["unrecognized_body"] = json!("body".repeat(256 * 1024));
        let mut inventory = host(vec![resource]);
        inventory.host = "host\n\"alias".into();
        inventory.error = Some("error\n".repeat(256 * 1024));
        let text = describe(&record(vec![inventory]), false);
        assert!(text.len() <= MAX_DESCRIPTION_BYTES);
        assert!(
            text.contains("host=\"host\\n\\\"alias\" tool=\"bash\" handle=\"shell\\n\\\"handle\"")
        );
        assert!(text.contains("instance_id=\"unique-instance\" pid=1234"));
        assert!(text.contains("process_exited=false output_closed=false"));
        assert!(text.contains("last-known inventory"));
        assert!(text.contains("not confirmed live"));
        assert_eq!(text.matches("bytes omitted]").count(), 2);
        assert!(!text.contains("unrecognized_body"));
    }

    #[test]
    fn current_and_previous_inventory_share_one_limit_with_exact_omission_counts() {
        let handles: Vec<_> = (0..100)
            .map(|index| shell(format!("shell-{index}"), "x".repeat(1024)))
            .collect();
        let mut observation = record(vec![host(handles.clone())]);
        observation.resumed = true;
        observation.unavailable_after_restart = vec![host(handles)];
        let text = describe(&observation, true);
        let included = text.matches(" tool=\"bash\" handle=").count();
        assert!(included > 0 && included < 200);
        assert!(text.len() <= MAX_DESCRIPTION_BYTES);
        assert!(text.contains("Saved conversation does not restore bash handles"));
        assert!(text.contains(&format!(
            "{} handles, and 0 model identities omitted",
            200 - included
        )));
        assert!(text.contains("omission does not mean resources are absent"));
        assert!(text.contains("bash action=list"));
        assert!(text.contains("session_history"));
    }

    #[test]
    fn oversized_identifiers_are_omitted_whole_and_counted() {
        let mut oversized_instance = shell("oversized-instance", "small");
        oversized_instance.details["instance_id"] = json!("instance".repeat(4096));
        let mut oversized_host = host(vec![shell("hidden-by-host", "small")]);
        oversized_host.host = "host".repeat(4096);
        let mut observation = record(vec![
            host(vec![
                shell("handle".repeat(4096), "small"),
                oversized_instance,
                shell("complete-handle", "small"),
            ]),
            oversized_host,
        ]);
        observation.model = ModelInfo::named("model".repeat(4096));
        let text = describe(&observation, false);
        assert!(text.len() <= MAX_DESCRIPTION_BYTES);
        assert!(text.contains("handle=\"complete-handle\""));
        assert!(!text.contains("oversized-instance"));
        assert!(!text.contains("hidden-by-host"));
        assert!(text.contains("1 hosts, 3 handles, and 1 model identities omitted"));
    }

    #[test]
    fn an_unavailable_empty_inventory_is_explicitly_only_last_known() {
        let mut inventory = host(vec![]);
        inventory.error = Some("offline".into());
        let text = describe(&record(vec![inventory]), false);
        assert!(text.contains("last-known inventory: no retained resources"));
        assert!(text.contains("not confirmed live"));
    }

    #[test]
    fn unknown_tool_body_is_not_rendered_and_editor_fingerprints_are_counted() {
        let text = describe(
            &record(vec![host(vec![
                ToolResource {
                    tool: "future-tool".into(),
                    id: "future-handle".into(),
                    details: json!({"body": "body".repeat(256 * 1024)}),
                },
                ToolResource {
                    tool: "str_replace_based_edit_tool".into(),
                    id: "/a/large/path".repeat(4096),
                    details: json!({}),
                },
            ])]),
            false,
        );
        assert!(text.len() <= MAX_DESCRIPTION_BYTES);
        assert!(text.contains("1 handles; 1 editor read fingerprints"));
        assert!(text.contains("tool=\"future-tool\" handle=\"future-handle\""));
        assert!(text.contains("inspect the owning tool for details"));
        assert!(!text.contains("Description limit:"));
    }
}
