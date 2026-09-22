//! Private, repeatable task evaluations over the same session runner as the server.
//! Case inputs and graders are frozen separately from each fresh run workspace.

mod case;
mod metrics;
mod run;

pub use case::{
    Case, CaseSource, CreateOptions, Workspace, create_case, discover_cases, load_case,
};
pub use run::{RunOptions, execute_job, report, run};

use serde::{Serialize, de::DeserializeOwned};
use std::path::Path;

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    crate::core::atomically_write(path, &bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))
}
