//! Each workspace owns its Git objects. The frozen recipe owns the source
//! snapshot needed to replay it after the original repository disappears.

use std::path::Path;

use crate::external_command::GIT;

use super::case::{Case, Workspace, copy_tree, git};
use super::write_json;

pub(super) async fn prepare(
    case: &mut Case,
    frozen: &Path,
    workspace: &Path,
) -> Result<(), String> {
    match &case.workspace {
        Workspace::Fixture => copy_tree(&frozen.join("workspace"), workspace)?,
        Workspace::Git { .. } => {
            std::fs::create_dir(workspace).map_err(|error| error.to_string())?;
        }
    }
    git(workspace, &["init", "--quiet"])?;
    if let Workspace::Git { repo, revision } = &case.workspace {
        fetch(workspace, &frozen.join(repo), revision).await?;
        git(workspace, &["checkout", "--detach", revision])?;
        freeze(case, frozen, workspace)?;
    }
    Ok(())
}

async fn fetch(workspace: &Path, source: &Path, revision: &str) -> Result<(), String> {
    let output = GIT
        .tokio_command()
        .arg("-C")
        .arg(workspace)
        .args(["fetch", "--no-tags", "--no-recurse-submodules", "--"])
        .arg(source)
        .arg(revision)
        .output()
        .await
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "prepare git workspace: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn freeze(case: &mut Case, frozen: &Path, workspace: &Path) -> Result<(), String> {
    // Always regenerate from the pinned checkout: the input may itself be a
    // portable case, or contain unrelated refs we do not need to preserve.
    let bundle = frozen.join("source.bundle");
    if bundle.exists() {
        std::fs::remove_file(&bundle).map_err(|error| error.to_string())?;
    }
    let bundle = bundle.to_str().ok_or("eval bundle path must be UTF-8")?;
    git(workspace, &["bundle", "create", bundle, "HEAD"])?;
    if let Workspace::Git { repo, .. } = &mut case.workspace {
        *repo = "source.bundle".into();
    }
    write_json(&frozen.join("case.json"), case)
}
