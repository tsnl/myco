//! Integration test: the interactive CLI is drivable over pipes (no TTY).
//!
//! This is the nested-agent contract: a supervisor starts `myco` itself inside
//! a bash session, writes one prompt per line, and reads turns off the
//! `USER n/m` headers. There is no dedicated subagent tool — the CLI is the
//! interface — so piped stdin must submit turns, slash commands must work, and
//! a failed model turn must return to the prompt instead of wedging the loop.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

/// Fresh `MYCO_HOME` + config for one test, removed on drop so a panicking
/// test cannot leak it.
struct PipeEnv {
    dir: PathBuf,
    config: PathBuf,
}

impl Drop for PipeEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The shared test config points at an unreachable gateway: a model turn must
/// fail fast and return to the prompt — a nested agent driver sees an ERROR
/// section, not a hang.
fn pipe_env(tag: &str) -> PipeEnv {
    let dir = std::env::temp_dir().join(format!("myco-{tag}-{}", uuid::Uuid::new_v4().as_simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        r#"model = "pipetest"

[models.pipetest]
protocol = "openai-responses"
base_url = "http://127.0.0.1:1/v1"
auth = { source = "none" }
context_window = 100000
"#,
    )
    .unwrap();
    PipeEnv { dir, config }
}

/// Spawn `myco` over pipes, feed it `input`, and return its stdout. Panics on
/// hang, spawn failure, or nonzero exit (with stdout+stderr in the message).
async fn run_myco(env: &PipeEnv, args: &[&str], input: &[u8]) -> String {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
        .args(args)
        .env("MYCO_HOME", &env.dir)
        .env("MYCO_CONFIG", &env.config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn myco");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(input).await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("piped REPL must not hang")
        .expect("wait myco");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "status={:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// The session id the CLI announces on exit (`session=<id>`, last one wins).
fn announced_session_id(stdout: &str) -> String {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("session="))
        .expect("session id announced on exit")
        .trim()
        .to_string()
}

/// Every stored session file under the sharded session store.
fn session_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("session"))
        .expect("session store exists")
        .flatten()
        .filter(|shard| shard.path().is_dir())
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect()
}

/// The stored session `id`, parsed.
fn session_json(dir: &Path, id: &str) -> serde_json::Value {
    let name = format!("{id}.json");
    let path = session_files(dir)
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == name.as_str()))
        .unwrap_or_else(|| panic!("session file {name} not found"));
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

/// Asserts exactly one session was stored and returns it parsed.
fn only_session(dir: &Path) -> serde_json::Value {
    let files = session_files(dir);
    assert_eq!(files.len(), 1, "{files:?}");
    serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap()
}

#[tokio::test]
async fn piped_repl_serves_turns_slash_commands_and_clean_exit() {
    let env = pipe_env("pipe-drive");

    // One line per turn: a model turn, a slash command, then quit.
    let stdout = run_myco(&env, &[], b"say hi\n/hosts\n/quit\n").await;

    // The failed model turn surfaces as ERROR, then the loop returns to a
    // fresh USER header — the turn boundary a nested-agent driver reads.
    assert!(stdout.contains("ERROR"), "{stdout}");
    assert!(stdout.matches("USER ").count() >= 2, "{stdout}");

    // Slash commands work over the pipe.
    assert!(stdout.contains("hosts: default=local"), "{stdout}");

    // No subagent tool in the catalog: nested agents ARE this piped interface.
    assert!(!stdout.contains("subagent"), "{stdout}");
}

/// `--parent-session` is the nested-agent lineage contract: the child's fresh
/// session lands in the shared store hidden (`kind: subagent`) and parented to
/// the supervisor, so default listings stay clean and the supervisor can read
/// it back by id.
#[tokio::test]
async fn parent_session_flag_creates_hidden_linked_session() {
    let env = pipe_env("parent-session");
    let parent_id = "cafef00dcafef00dcafef00dcafef00d";

    // Sessions persist after the first turn (a zero-turn /quit writes nothing),
    // so run one — the unreachable gateway makes it fail fast, which still
    // records the user message and force-saves on turn end.
    let _ = run_myco(&env, &["--parent-session", parent_id], b"hello\n/quit\n").await;

    // Exactly one session was written; it is hidden and parented.
    let session = only_session(&env.dir);
    assert_eq!(session["kind"], "subagent", "{session}");
    assert_eq!(session["parent_session_id"], parent_id, "{session}");
}

/// `--parent-session … --fork` is the context-fork contract: the child's fresh
/// hidden session is seeded with the parent's saved conversation under a new
/// id, and the inherited transcript is NOT replayed to the pipe (a supervisor
/// must never read its own context back as child output).
#[tokio::test]
async fn fork_seeds_child_with_parent_conversation() {
    let env = pipe_env("fork");

    // Parent run: one (failing) turn is enough — the user message is
    // checkpointed to disk the moment it is submitted.
    let parent_stdout = run_myco(&env, &[], b"parent-marker-alpha\n/quit\n").await;
    let parent_id = announced_session_id(&parent_stdout);

    // Fork a child off the stored parent session.
    let child_stdout = run_myco(
        &env,
        &["--parent-session", &parent_id, "--fork"],
        b"child-marker-beta\n/quit\n",
    )
    .await;
    let child_id = announced_session_id(&child_stdout);
    assert_ne!(child_id, parent_id, "fork must mint a new session id");
    assert!(
        !child_stdout.contains("parent-marker-alpha"),
        "inherited transcript must not replay to the pipe:\n{child_stdout}"
    );

    // The child session file: hidden, parented, seeded with the parent's
    // conversation plus its own turn.
    let session = session_json(&env.dir, &child_id);
    assert_eq!(session["kind"], "subagent", "{session}");
    assert_eq!(session["parent_session_id"], parent_id, "{session}");
    let messages = serde_json::to_string(&session["messages"]).unwrap();
    assert!(messages.contains("parent-marker-alpha"), "{messages}");
    assert!(messages.contains("child-marker-beta"), "{messages}");
}

/// Every session tells the agent which session it is, in the conversation
/// rather than the system prompt: the id is stamped on the first user message
/// of a fresh session, and a context fork — which mints a new id but inherits
/// the parent's stamped message — opens the first message it adds with a fork
/// block naming itself, its parent, and what it inherited.
///
/// Stamp *content* is owned by the prompts/bin unit tests; this test proves
/// the end-to-end residue: the stamp lands in the stored session file, as
/// content block [0] ahead of the user's words, exactly once per conversation.
#[tokio::test]
async fn session_id_is_stamped_on_the_first_user_message() {
    let env = pipe_env("stamp");

    // The text blocks of one stored user message, in order.
    let user_texts = |message: &serde_json::Value| -> Vec<String> {
        message["UserMessage"]["content"]
            .as_array()
            .expect("user message content")
            .iter()
            .filter_map(|c| Some(c["Text"]["text"].as_str()?.to_string()))
            .collect()
    };

    // A fresh session: the stamp leads its first message, ahead of the words
    // the user typed. The failing turns are enough — each user message is
    // checkpointed the moment it is submitted.
    let stdout = run_myco(&env, &[], b"parent-marker-alpha\nsecond-turn\n/quit\n").await;
    let parent_id = announced_session_id(&stdout);
    let parent = session_json(&env.dir, &parent_id);
    let first = user_texts(&parent["messages"][0]);
    assert!(first[0].starts_with("# Session"), "{first:?}");
    assert!(first[0].contains(&parent_id), "{first:?}");
    assert!(first[1].contains("parent-marker-alpha"), "{first:?}");
    // The first message only: later turns carry the user's words alone, so
    // one conversation states its session once.
    let later = parent["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m.get("UserMessage").is_some())
        .map(user_texts)
        .find(|texts| texts.iter().any(|t| t.contains("second-turn")))
        .expect("the second turn");
    assert!(!later.iter().any(|t| t.contains("# Session")), "{later:?}");

    // A context fork: the inherited message still names the parent, and the
    // child opens the turn it adds with a fork block of its own.
    let child_stdout = run_myco(
        &env,
        &["--parent-session", &parent_id, "--fork"],
        b"child-marker-beta\n/quit\n",
    )
    .await;
    let child_id = announced_session_id(&child_stdout);
    assert_ne!(child_id, parent_id, "fork must mint a new session id");
    let child = session_json(&env.dir, &child_id);
    let messages = child["messages"].as_array().expect("messages");
    let inherited = user_texts(&messages[0]);
    assert!(inherited[0].contains(&parent_id), "{inherited:?}");
    let own = messages
        .iter()
        .map(user_texts)
        .find(|texts| texts.iter().any(|t| t.contains("child-marker-beta")))
        .expect("the fork's own turn");
    // A fork says what it is, rather than re-stamping `# Session` and leaving
    // the child to work out that the newest of two blocks is the live one.
    assert!(own[0].starts_with("# Fork"), "{own:?}");
    assert!(!own[0].starts_with("# Session"), "{own:?}");
    // It names the live session (itself), the session it inherited from, and
    // what the inherited messages are. Which boundary is the unit tests' to
    // pin down; that one is stated at all is this test's residue.
    assert!(own[0].contains(&child_id), "{own:?}");
    assert!(own[0].contains(&parent_id), "{own:?}");
    assert!(own[0].contains("Inherited context ends"), "{own:?}");
    // The directive stays a block of its own — the fork appends, never edits.
    assert!(own[1].contains("child-marker-beta"), "{own:?}");
}
