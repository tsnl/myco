//! Integration test: the interactive CLI is drivable over pipes (no TTY).
//!
//! This is the nested-agent contract: a supervisor starts `myco` itself inside
//! a bash session, writes one prompt per line, and reads turns off the
//! `USER n/m` headers. There is no dedicated subagent tool — the CLI is the
//! interface — so piped stdin must submit turns, slash commands must work, and
//! a failed model turn must return to the prompt instead of wedging the loop.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn piped_repl_serves_turns_slash_commands_and_clean_exit() {
    let dir = std::env::temp_dir().join(format!(
        "myco-pipe-drive-{}",
        uuid::Uuid::new_v4().as_simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    // Unreachable gateway: the model turn must fail fast and return to the
    // prompt — a nested agent driver sees an ERROR section, not a hang.
    std::fs::write(
        &config_path,
        r#"model = "pipetest"

[models.pipetest]
protocol = "openai-responses"
base_url = "http://127.0.0.1:1/v1"
auth = { source = "none" }
context_window = 100000
"#,
    )
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
        .env("MYCO_HOME", &dir)
        .env("MYCO_CONFIG", &config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn myco");

    // One line per turn: a model turn, a slash command, then quit.
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(b"say hi\n/hosts\n/quit\n").await.unwrap();
    drop(stdin);

    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("piped REPL must not hang")
        .expect("wait myco");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );

    // The failed model turn surfaces as ERROR, then the loop returns to a
    // fresh USER header — the turn boundary a nested-agent driver reads.
    assert!(stdout.contains("ERROR"), "{stdout}");
    assert!(stdout.matches("USER ").count() >= 2, "{stdout}");

    // Slash commands work over the pipe.
    assert!(stdout.contains("hosts: default=local"), "{stdout}");

    // No subagent tool in the catalog: nested agents ARE this piped interface.
    assert!(!stdout.contains("subagent"), "{stdout}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--parent-session` is the nested-agent lineage contract: the child's fresh
/// session lands in the shared store hidden (`kind: subagent`) and parented to
/// the supervisor, so default listings stay clean and the supervisor can read
/// it back by id.
#[tokio::test]
async fn parent_session_flag_creates_hidden_linked_session() {
    let dir = std::env::temp_dir().join(format!(
        "myco-parent-session-{}",
        uuid::Uuid::new_v4().as_simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"model = "pipetest"

[models.pipetest]
protocol = "openai-responses"
base_url = "http://127.0.0.1:1/v1"
auth = { source = "none" }
context_window = 100000
"#,
    )
    .unwrap();

    let parent_id = "cafef00dcafef00dcafef00dcafef00d";
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
        .args(["--parent-session", parent_id])
        .env("MYCO_HOME", &dir)
        .env("MYCO_CONFIG", &config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn myco");

    // Sessions persist after the first turn (a zero-turn /quit writes nothing),
    // so run one — the unreachable gateway makes it fail fast, which still
    // records the user message and force-saves on turn end.
    let mut stdin = child.stdin.take().expect("stdin");
    stdin.write_all(b"hello\n/quit\n").await.unwrap();
    drop(stdin);

    let output = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("piped REPL must not hang")
        .expect("wait myco");
    assert!(
        output.status.success(),
        "status={:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactly one session was written; it is hidden and parented.
    let session_files: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("session"))
        .expect("session store exists")
        .flatten()
        .filter(|shard| shard.path().is_dir())
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
        .map(|f| f.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(session_files.len(), 1, "{session_files:?}");
    let session: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session_files[0]).unwrap()).unwrap();
    assert_eq!(session["kind"], "subagent", "{session}");
    assert_eq!(session["parent_session_id"], parent_id, "{session}");

    let _ = std::fs::remove_dir_all(&dir);
}
