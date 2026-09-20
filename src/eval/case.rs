use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::validate_context;
use crate::core::image_store::ImageStore;
use crate::external_command::GIT;
use crate::generative_model::{Content, Message};
use crate::session::Session;

use super::{read_json, write_json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub version: u32,
    pub name: String,
    pub split: String,
    pub workspace: Workspace,
    /// Runs outside the model. `{case}` expands to the frozen case directory.
    pub grader: Vec<String>,
    pub source: Option<CaseSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Workspace {
    Fixture,
    Git { repo: PathBuf, revision: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseSource {
    pub session_id: String,
    pub thread_id: String,
    pub user_message: usize,
}

pub fn load_case(path: &Path) -> Result<Case, String> {
    let case: Case = read_json(&path.join("case.json"))?;
    if case.version != 1 {
        return Err("unsupported eval case version; expected 1".into());
    }
    if !["train", "validation", "test"].contains(&case.split.as_str()) {
        return Err("split must be train, validation, or test".into());
    }
    if case.name.trim().is_empty() || case.grader.is_empty() {
        return Err("case needs a name and grader command".into());
    }
    if let Workspace::Git { repo, revision } = &case.workspace
        && (!repo.is_absolute()
            || revision.len() != 40
            || !revision.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(
            "git cases require an absolute local repository and a pinned 40-character commit"
                .into(),
        );
    }
    let context: Vec<Message> = read_json(&path.join("context.json"))?;
    validate_context(&context).map_err(|error| error.to_string())?;
    if !context.last().is_some_and(Message::is_user_turn) {
        return Err("eval context must end at a real user task, before the answer".into());
    }
    Ok(case)
}

pub fn discover_cases(path: &Path) -> Result<Vec<PathBuf>, String> {
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    if path.join("case.json").is_file() {
        load_case(&path)?;
        return Ok(vec![path]);
    }
    let mut cases = vec![];
    for entry in std::fs::read_dir(&path).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() && path.join("case.json").is_file() {
            load_case(&path)?;
            cases.push(path);
        }
    }
    cases.sort();
    if cases.is_empty() {
        return Err(format!("no eval cases in {}", path.display()));
    }
    Ok(cases)
}

/// Export only the prefix ending at the selected human input. The later answer,
/// session metadata, and unrelated threads never enter the case.
pub fn session_context(
    session: &Session,
    thread_id: Option<&str>,
    user_message: usize,
) -> Result<(Vec<Message>, CaseSource), String> {
    let thread = match thread_id {
        Some(id) => session
            .threads()
            .iter()
            .find(|thread| thread.id == id)
            .ok_or("unknown thread")?,
        None => session.active_thread(),
    };
    if !thread
        .messages
        .get(user_message)
        .is_some_and(Message::is_user_turn)
    {
        return Err("--user-message must identify a human input message (zero-based index from session_history)".into());
    }
    let mut context = thread.messages[..=user_message].to_vec();
    // Old runtime identities describe handles this fresh run does not own.
    for message in &mut context {
        if let Message::UserMessage { content } = message {
            content.retain(
                |part| !matches!(part, Content::System { kind, .. } if kind != "compaction"),
            );
        }
    }
    context.retain(
        |message| !matches!(message, Message::UserMessage { content } if content.is_empty()),
    );
    validate_context(&context).map_err(|error| error.to_string())?;
    Ok((
        context,
        CaseSource {
            session_id: session.id.clone(),
            thread_id: thread.id.clone(),
            user_message,
        },
    ))
}

pub struct CreateOptions {
    pub output: PathBuf,
    pub task: Option<String>,
    pub session: Option<String>,
    pub thread: Option<String>,
    pub user_message: Option<usize>,
    pub fixture: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    pub revision: Option<String>,
    pub grader: PathBuf,
    pub split: String,
}

pub fn create_case(options: CreateOptions) -> Result<PathBuf, String> {
    let (mut context, source) = match (&options.task, &options.session) {
        (Some(task), None) if !task.trim().is_empty() => (
            vec![Message::UserMessage {
                content: vec![Content::Text { text: task.clone() }],
            }],
            None,
        ),
        (None, Some(id)) => {
            let session = Session::load_by_id_or_prefix(id)?;
            let (context, source) = session_context(
                &session,
                options.thread.as_deref(),
                options
                    .user_message
                    .ok_or("from-session requires --user-message")?,
            )?;
            (context, Some(source))
        }
        _ => return Err("provide exactly one task or source session".into()),
    };
    if options.output.exists() {
        return Err(format!(
            "case output already exists: {}",
            options.output.display()
        ));
    }
    let grader = options
        .grader
        .canonicalize()
        .map_err(|error| format!("grader: {error}"))?;
    let workspace = match (&options.fixture, &options.repo, &options.revision) {
        (Some(_), None, None) => Workspace::Fixture,
        (None, Some(repo), Some(revision)) => {
            let repo = repo.canonicalize().map_err(|error| error.to_string())?;
            let revision = git(
                &repo,
                &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
            )?;
            Workspace::Git {
                repo,
                revision: revision.trim().into(),
            }
        }
        _ => return Err("provide --workspace DIR or --repo DIR with --revision COMMIT".into()),
    };
    if !["train", "validation", "test"].contains(&options.split.as_str()) {
        return Err("split must be train, validation, or test".into());
    }
    let output = std::path::absolute(&options.output).map_err(|error| error.to_string())?;
    if let Some(fixture) = &options.fixture {
        let fixture = fixture.canonicalize().map_err(|error| error.to_string())?;
        if output.starts_with(&fixture) {
            return Err("case output must be outside its workspace fixture".into());
        }
    }
    std::fs::create_dir_all(output.parent().ok_or("case needs a parent directory")?)
        .map_err(|error| error.to_string())?;
    std::fs::create_dir(&output).map_err(|error| error.to_string())?;
    let result = (|| {
        if let Some(fixture) = &options.fixture {
            copy_tree(fixture, &output.join("workspace"))?;
        }
        let profile_store = ImageStore::for_profile()?;
        let case_store = ImageStore::new(output.join("images"));
        for part in context.iter().flat_map(Message::content) {
            if let Content::Image { source } = part {
                profile_store.copy_reference(source, &case_store)?;
            }
        }
        case_store.externalize_messages(&mut context)?;
        std::fs::copy(grader, output.join("grader.py")).map_err(|error| error.to_string())?;
        write_json(&output.join("context.json"), &context)?;
        let case = Case {
            version: 1,
            name: output.file_name().unwrap().to_string_lossy().into_owned(),
            split: options.split,
            workspace,
            grader: vec!["python3".into(), "{case}/grader.py".into()],
            source,
        };
        write_json(&output.join("case.json"), &case)?;
        load_case(&output)?;
        Ok(output.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&output);
    }
    result
}

pub(super) fn git(repo: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = GIT
        .command()
        .arg("-C")
        .arg(repo)
        .args(arguments)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("git: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::create_dir_all(target).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if [".git", ".myco", "target", "__pycache__"]
            .iter()
            .any(|skip| name == *skip)
        {
            continue;
        }
        let dest = target.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            return Err(format!(
                "fixture contains symlink {}; use a pinned git workspace or replace it with a file",
                entry.path().display()
            ));
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), dest).map_err(|error| error.to_string())?;
        } else {
            return Err(format!(
                "fixture contains a non-file: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}
