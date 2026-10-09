//! Discover metadata on the host that owns the paths; instructions stay on disk.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::core::Async;
use crate::generative_model::{ToolResult, ToolSpec, ToolUse};
use crate::skills::SkillCatalog;

use super::{HostDispatchContext, ToolService, tool_input_schema};

const MAX_OBSERVED_DIRECTORIES: usize = 16;
#[derive(Default)]
struct Observations {
    thread_id: Option<String>,
    directories: VecDeque<(PathBuf, SkillCatalog)>,
}
type OwnerObservations = Arc<tokio::sync::Mutex<Observations>>;

//
// Host-local discovery and owner lifetime
//

pub struct SkillsService {
    directory: Result<PathBuf, String>,
    home: Option<PathBuf>,
    owners: Mutex<HashMap<uuid::Uuid, OwnerObservations>>,
}

impl Default for SkillsService {
    fn default() -> Self {
        Self {
            directory: std::env::current_dir().map_err(|error| error.to_string()),
            home: dirs::home_dir(),
            owners: Mutex::new(HashMap::new()),
        }
    }
}

impl SkillsService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn specs() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "skills".into(),
            description: "Discover skill metadata in a directory and its project ancestors on this host. Omit path for the host worker's launch directory. Scans .agents/skills, .claude/skills and .grok/skills plus user skill directories, with bounded work. Use after shell cd to discover skills where you work; shell directory changes are not inferred. Read a relevant SKILL.md with the editor before following it. Discovery does not grant permissions.".into(),
            input_schema: tool_input_schema::<Input>(),
        }]
    }

    fn observations(&self, owner: uuid::Uuid) -> OwnerObservations {
        self.owners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(owner)
            .or_default()
            .clone()
    }

    async fn discover(
        &self,
        target: Target,
        ctx: &HostDispatchContext,
        force: bool,
    ) -> Result<Option<String>, String> {
        let owners = self.observations(ctx.agent_id);
        let observed = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err("skill discovery cancelled".into()),
            observed = owners.lock_owned() => observed,
        };
        let directory = self.directory.clone()?;
        let home = self.home.clone();
        let scan = move || {
            let path = target.directory(directory)?;
            let catalog = SkillCatalog::scan(&path, home.as_deref());
            // A cancelled caller cannot abort blocking filesystem IO. Keep the
            // owner gate inside that work so retries cannot pile up more scans.
            Ok::<_, String>((observed, path, catalog))
        };
        let (mut observed, path, catalog) = run_scan(&ctx.cancel, scan).await?;
        if observed.thread_id != ctx.thread_id {
            observed.directories.clear();
            observed.thread_id = ctx.thread_id.clone();
        }
        Ok(record(&mut observed.directories, path, catalog, force))
    }
}

/// A blocking job owns its scan gate until it exits, even if its caller cancels.
async fn run_scan<T: Send + 'static>(
    cancel: &crate::core::CancelToken,
    scan: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    // In-process workers also support synchronous embedders, like the editor.
    let result = if tokio::runtime::Handle::try_current().is_ok() {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("skill discovery cancelled".into()),
            result = tokio::task::spawn_blocking(scan) => result.map_err(|error| format!("skill discovery worker failed: {error}"))?,
        }
    } else {
        scan()
    };
    if cancel.is_cancelled() {
        return Err("skill discovery cancelled".into());
    }
    result
}

impl ToolService for SkillsService {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        Self::specs()
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool: ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool.input) {
                Ok(input) => input,
                Err(error) => return ToolResult::err(format!("invalid skills input: {error}")),
            };
            let target = Target::Directory(input.path);
            match self.discover(target, &ctx, true).await {
                Ok(Some(text)) => ToolResult::text(text),
                Ok(None) => unreachable!("explicit discovery always returns a catalog"),
                Err(error) => ToolResult::err(error),
            }
        })
    }

    fn observe_successful_call(
        self: Arc<Self>,
        tool: ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<Option<String>> {
        Box::pin(async move {
            let target = Target::from_tool(&tool)?;
            match self.discover(target, &ctx, false).await {
                Ok(notice) => notice,
                Err(_) if ctx.cancel.is_cancelled() => None,
                Err(error) => Some(format!("[myco: Skill discovery incomplete] {error}")),
            }
        })
    }

    fn on_agent_finished(&self, owner: uuid::Uuid) {
        self.owners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&owner);
    }
}

//
// Bounded observations and operation paths
//

fn record(
    observed: &mut VecDeque<(PathBuf, SkillCatalog)>,
    path: PathBuf,
    catalog: SkillCatalog,
    force: bool,
) -> Option<String> {
    let previous = observed
        .iter()
        .position(|(directory, _)| directory == &path)
        .and_then(|index| observed.remove(index));
    let changed = previous.as_ref().is_none_or(|(_, old)| old != &catalog);
    let empty = catalog.entries.is_empty() && catalog.issues.is_empty() && !catalog.truncated;
    let notice = (force || (changed && (previous.is_some() || !empty))).then(|| {
        format!(
            "[myco: Skills for directory {:?}]\n{}",
            path,
            catalog.render_notice()
        )
    });
    observed.push_back((path, catalog));
    if observed.len() > MAX_OBSERVED_DIRECTORIES {
        observed.pop_front();
    }
    notice
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Directory to discover skills for, absolute or relative to this host's launch directory.
    #[serde(default)]
    path: Option<String>,
}

enum Target {
    Directory(Option<String>),
    OperatedPath(String),
}

impl Target {
    fn from_tool(tool: &ToolUse) -> Option<Self> {
        match tool.name.as_str() {
            "bash" => Some(Self::Directory(None)),
            "str_replace_based_edit_tool" | "view_image" => tool
                .input
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(|path| Self::OperatedPath(path.to_owned())),
            _ => None,
        }
    }

    fn directory(self, launch: PathBuf) -> Result<PathBuf, String> {
        let operated = matches!(self, Self::OperatedPath(_));
        let path = match self {
            Self::Directory(None) => launch,
            Self::Directory(Some(path)) | Self::OperatedPath(path) => {
                if path.trim().is_empty() {
                    return Err("skills path must be a non-empty directory".into());
                }
                launch.join(path)
            }
        };
        // The successful tool selected this path. Resolve its actual host-local
        // target before the scanner opens skill directories without symlinks.
        let path = path
            .canonicalize()
            .map_err(|error| format!("cannot discover skills at {path:?}: {error}"))?;
        if path.is_dir() {
            return Ok(path);
        }
        if operated && let Some(parent) = path.parent() {
            return Ok(parent.to_path_buf());
        }
        Err(format!("skills path is not a directory: {path:?}"))
    }
}

#[cfg(test)]
mod tests;
