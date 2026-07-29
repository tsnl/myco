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

/// `--parent-session … --fork` is the context-fork contract: the child's fresh
/// hidden session is seeded with the parent's saved conversation under a new
/// id, and the inherited transcript is NOT replayed to the pipe (a supervisor
/// must never read its own context back as child output).
#[tokio::test]
async fn fork_seeds_child_with_parent_conversation() {
    let dir = std::env::temp_dir().join(format!("myco-fork-{}", uuid::Uuid::new_v4().as_simple()));
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

    let run = |args: Vec<String>, input: &'static [u8]| {
        let dir = dir.clone();
        let config_path = config_path.clone();
        async move {
            let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
                .args(args)
                .env("MYCO_HOME", &dir)
                .env("MYCO_CONFIG", &config_path)
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
            assert!(
                output.status.success(),
                "status={:?}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    };

    // Parent run: one (failing) turn is enough — the user message is
    // checkpointed to disk the moment it is submitted.
    let parent_stdout = run(vec![], b"parent-marker-alpha\n/quit\n").await;
    let parent_id = parent_stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("session="))
        .expect("parent session id announced on exit")
        .trim()
        .to_string();

    // Fork a child off the stored parent session.
    let child_stdout = run(
        vec![
            "--parent-session".into(),
            parent_id.clone(),
            "--fork".into(),
        ],
        b"child-marker-beta\n/quit\n",
    )
    .await;
    let child_id = child_stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("session="))
        .expect("child session id announced on exit")
        .trim()
        .to_string();
    assert_ne!(child_id, parent_id, "fork must mint a new session id");
    assert!(
        !child_stdout.contains("parent-marker-alpha"),
        "inherited transcript must not replay to the pipe:\n{child_stdout}"
    );

    // The child session file: hidden, parented, seeded with the parent's
    // conversation plus its own turn.
    let child_path = std::fs::read_dir(dir.join("session"))
        .expect("session store exists")
        .flatten()
        .filter(|shard| shard.path().is_dir())
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n == format!("{child_id}.json").as_str())
        })
        .expect("child session file");
    let session: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&child_path).unwrap()).unwrap();
    assert_eq!(session["kind"], "subagent", "{session}");
    assert_eq!(session["parent_session_id"], parent_id, "{session}");
    let messages = serde_json::to_string(&session["messages"]).unwrap();
    assert!(messages.contains("parent-marker-alpha"), "{messages}");
    assert!(messages.contains("child-marker-beta"), "{messages}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every session tells the agent which session it is, in the conversation
/// rather than the system prompt: the id is stamped on the first user message
/// of a fresh session, and a context fork — which mints a new id but inherits
/// the parent's stamped message — stamps its own on the first message it adds.
#[tokio::test]
async fn session_id_is_stamped_on_the_first_user_message() {
    let dir = std::env::temp_dir().join(format!("myco-stamp-{}", uuid::Uuid::new_v4().as_simple()));
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

    let run = |args: Vec<String>, input: &'static [u8]| {
        let dir = dir.clone();
        let config_path = config_path.clone();
        async move {
            let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
                .args(args)
                .env("MYCO_HOME", &dir)
                .env("MYCO_CONFIG", &config_path)
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
            stdout
                .lines()
                .rev()
                .find_map(|l| l.trim().strip_prefix("session="))
                .expect("session id announced on exit")
                .trim()
                .to_string()
        }
    };

    let session_json = |id: &str| {
        let path = std::fs::read_dir(dir.join("session"))
            .expect("session store exists")
            .flatten()
            .filter(|shard| shard.path().is_dir())
            .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
            .map(|f| f.path())
            .find(|p| {
                p.file_name()
                    .is_some_and(|n| n == format!("{id}.json").as_str())
            })
            .expect("session file");
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
    };
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
    let parent_id = run(vec![], b"parent-marker-alpha\nsecond-turn\n/quit\n").await;
    let parent = session_json(&parent_id);
    let first = user_texts(&parent["messages"][0]);
    assert!(first[0].starts_with("# Session"), "{first:?}");
    assert!(first[0].contains(&parent_id), "{first:?}");
    assert!(first[1].contains("parent-marker-alpha"), "{first:?}");
    // Where the agent is running, not just which session: the start time and
    // the directory myco was launched in (this test process's own cwd, which
    // the spawned binary inherits).
    assert!(first[0].contains("- Started: "), "{first:?}");
    let cwd = std::env::current_dir().unwrap();
    assert!(
        first[0].contains(&format!("`{}`", cwd.display())),
        "{first:?}"
    );
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
    // child stamps its own id on the turn it adds.
    let child_id = run(
        vec![
            "--parent-session".into(),
            parent_id.clone(),
            "--fork".into(),
        ],
        b"child-marker-beta\n/quit\n",
    )
    .await;
    assert_ne!(child_id, parent_id, "fork must mint a new session id");
    let child = session_json(&child_id);
    let messages = child["messages"].as_array().expect("messages");
    let inherited = user_texts(&messages[0]);
    assert!(inherited[0].contains(&parent_id), "{inherited:?}");
    let own = messages
        .iter()
        .map(user_texts)
        .find(|texts| texts.iter().any(|t| t.contains("child-marker-beta")))
        .expect("the fork's own turn");
    assert!(own[0].starts_with("# Session"), "{own:?}");
    assert!(own[0].contains(&child_id), "{own:?}");

    let _ = std::fs::remove_dir_all(&dir);
}
