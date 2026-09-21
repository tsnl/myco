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

mod test_utils;

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
        .env("MYCO_PROFILE", "default")
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
    std::fs::read_dir(dir.join("profiles/default/session"))
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
async fn turn_times_replay_after_restart_and_archive_restore_are_explicit() {
    let env = pipe_env("visibility");
    let stdout = run_myco(
        &env,
        &[],
        b"task\n/archive\n/session\n/sessions archived\n/restore\n/session\n/quit\n",
    )
    .await;
    assert!(stdout.contains("archived:  true"), "{stdout}");
    assert!(stdout.contains("archived:  false"), "{stdout}");
    assert!(stdout.contains("[archived]"), "{stdout}");
    let session = only_session(&env.dir);
    assert_eq!(session["version"], myco::SESSION_FILE_VERSION);
    let time = chrono::DateTime::parse_from_rfc3339(
        session["threads"][0]["user_turn_timestamps"]["0"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let line = myco::tui::transcript::acceptance_line(Some(time.with_timezone(&chrono::Utc)));
    assert!(stdout.contains(&line), "{stdout}");
    let replay = run_myco(
        &env,
        &["--resume", session["id"].as_str().unwrap()],
        b"/quit\n",
    )
    .await;
    assert!(replay.contains(&line), "{replay}");
    assert!(!replay.contains("# Session"), "{replay}");
    assert!(!replay.contains("Launch directory:"), "{replay}");
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

#[tokio::test]
async fn profile_selection_reaches_nested_agents_after_a_cwd_change() {
    use serde_json::json;
    use test_utils::StubHttpServer;

    let env = pipe_env("profiles");
    let root = env.dir.join("installation");
    let profile = root.join("profiles/research");
    let parent = myco::Session::new_with_id("pipetest", "cafef00dcafef00dcafef00dcafef00d");
    let store = profile.join("session/ca");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join(format!("{}.json", parent.id)),
        serde_json::to_vec(&parent).unwrap(),
    )
    .unwrap();
    let executable = env!("CARGO_BIN_EXE_myco").replace('\'', "'\\''");
    let arguments = json!({
        "command": format!("cd / && '{executable}' -p child --parent-session {}", parent.id),
    });
    let done = || {
        StubHttpServer::sse_response(vec![
            json!({"type":"response.output_text.delta", "delta":"done"}),
            json!({"type":"response.completed", "response":{"status":"completed"}}),
        ])
    };
    let server = StubHttpServer::sequence(vec![
        StubHttpServer::sse_response(vec![
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"function_call", "name":"bash", "call_id":"nested", "arguments":""}}),
            json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":arguments.to_string()}),
            json!({"type":"response.completed", "response":{"status":"completed"}}),
        ]),
        done(),
        done(),
    ]).await;
    let config = std::fs::read_to_string(&env.config)
        .unwrap()
        .replace("http://127.0.0.1:1/v1", &server.base_url());
    std::fs::write(profile.join("config.toml"), config).unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .args([
                "--profile",
                "research",
                "--resume",
                &parent.id,
                "-p",
                "parent",
            ])
            .current_dir(&env.dir)
            .env("MYCO_HOME", "installation")
            .env("MYCO_PROFILE", "wrong")
            .env_remove("MYCO_CONFIG")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("nested profile run stalled")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.connections(),
        3,
        "parent, child, parent continuation: {}",
        std::fs::read_to_string(store.join(format!("{}.json", parent.id))).unwrap()
    );
    let sessions: Vec<myco::Session> = std::fs::read_dir(profile.join("session"))
        .unwrap()
        .flatten()
        .flat_map(|shard| std::fs::read_dir(shard.path()).unwrap().flatten())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| myco::Session::load(&entry.path()).unwrap())
        .collect();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .any(|session| session.parent_session_id.as_deref() == Some(&parent.id))
    );
    assert!(!root.join("profiles/wrong").exists());
    assert!(!root.join("profiles/default").exists());
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
    let messages = serde_json::to_string(&session["threads"][0]["messages"]).unwrap();
    assert!(messages.contains("parent-marker-alpha"), "{messages}");
    assert!(messages.contains("child-marker-beta"), "{messages}");
}

/// Every session tells the agent which session it is, in the conversation
/// rather than the system prompt: the id is stamped on the first user message
/// of a fresh session, and a context fork — which mints a new id but inherits
/// the parent's stamped message — stamps its own on the first message it adds.
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
            .filter_map(|c| {
                c["Text"]["text"]
                    .as_str()
                    .or_else(|| c["System"]["text"].as_str())
                    .map(str::to_owned)
            })
            .collect()
    };

    // A fresh session: the stamp leads its first message, ahead of the words
    // the user typed. The failing turns are enough — each user message is
    // checkpointed the moment it is submitted.
    let stdout = run_myco(&env, &[], b"parent-marker-alpha\nsecond-turn\n/quit\n").await;
    let parent_id = announced_session_id(&stdout);
    let parent = session_json(&env.dir, &parent_id);
    let first = user_texts(&parent["threads"][0]["messages"][0]);
    assert!(first[0].starts_with("# Session"), "{first:?}");
    assert!(first[0].contains(&parent_id), "{first:?}");
    assert!(first[1].contains("parent-marker-alpha"), "{first:?}");
    assert_eq!(
        parent["threads"][0]["messages"][0]["UserMessage"]["content"][0]["System"]["kind"],
        "session"
    );
    // The first message only: later turns carry the user's words alone, so
    // one conversation states its session once.
    let later = parent["threads"][0]["messages"]
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
    let child_stdout = run_myco(
        &env,
        &["--parent-session", &parent_id, "--fork"],
        b"child-marker-beta\n/quit\n",
    )
    .await;
    let child_id = announced_session_id(&child_stdout);
    assert_ne!(child_id, parent_id, "fork must mint a new session id");
    let child = session_json(&env.dir, &child_id);
    let messages = child["threads"][0]["messages"]
        .as_array()
        .expect("messages");
    let inherited = user_texts(&messages[0]);
    assert!(inherited[0].contains(&parent_id), "{inherited:?}");
    let own = messages
        .iter()
        .map(user_texts)
        .find(|texts| texts.iter().any(|t| t.contains("child-marker-beta")))
        .expect("the fork's own turn");
    assert!(own[0].starts_with("# Session"), "{own:?}");
    assert!(own[0].contains(&child_id), "{own:?}");
}

#[tokio::test]
async fn effort_changes_and_restart_are_durable_without_appearing_in_transcript_replay() {
    let env = pipe_env("runtime-notices");
    let first = run_myco(&env, &[], b"record this task\n/effort low\n/quit\n").await;
    let id = announced_session_id(&first);
    let saved =
        myco::Session::from_json(&serde_json::to_vec(&session_json(&env.dir, &id)).unwrap())
            .unwrap();
    let record = myco::RuntimeRecord::latest(&saved.active_thread().messages).unwrap();
    assert_eq!(
        record.model.effort,
        Some(myco::generative_model::Effort::Low)
    );
    assert_eq!(
        record.previous_model.unwrap().effort,
        Some(myco::generative_model::Effort::High)
    );
    let times = saved.active_thread().user_turn_timestamps.clone();
    let replay = run_myco(&env, &["--resume", &id, "--effort", "max"], b"/quit\n").await;
    assert!(replay.contains("record this task"));
    for output in [&first, &replay] {
        assert!(!output.contains("Current runtime observations"), "{output}");
        assert!(!output.contains("Session resumed; use"), "{output}");
        assert!(!output.contains("Model or effort changed"), "{output}");
    }
    let saved =
        myco::Session::from_json(&serde_json::to_vec(&session_json(&env.dir, &id)).unwrap())
            .unwrap();
    let resumed = myco::RuntimeRecord::latest(&saved.active_thread().messages).unwrap();
    assert_ne!(resumed.runtime_id, record.runtime_id);
    assert!(resumed.resumed);
    assert_eq!(
        resumed.model.effort,
        Some(myco::generative_model::Effort::Max)
    );
    assert_eq!(saved.active_thread().user_turn_timestamps, times);
}

fn model_answer(text: &str, input_tokens: u64) -> Vec<u8> {
    test_utils::StubHttpServer::sse_response(vec![
        serde_json::json!({"type":"response.output_text.delta", "delta":text}),
        serde_json::json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":input_tokens,"output_tokens":1}
        }}),
    ])
}

fn compact_test_session(env: &PipeEnv) -> myco::Session {
    let session = myco::Session::new("pipetest");
    let store = env
        .dir
        .join("profiles/default/session")
        .join(&session.id[..2]);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join(format!("{}.json", session.id)),
        serde_json::to_vec(&session).unwrap(),
    )
    .unwrap();
    session
}

fn write_summary_response(session: &myco::Session) -> Vec<u8> {
    use serde_json::json;
    let arguments = json!({
        "action":"write_summary", "session_id":session.id,
        "thread_id":session.active_thread().id,
        "markdown":"# Goal / active task\nFinish the pending task."
    });
    model_tool("session_history", arguments, 100)
}

fn model_tool(name: &str, arguments: serde_json::Value, input_tokens: u64) -> Vec<u8> {
    use serde_json::json;
    test_utils::StubHttpServer::sse_response(vec![
        json!({"type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call", "name":name, "call_id":"tool", "arguments":""}}),
        json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":arguments.to_string()}),
        json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":input_tokens,"output_tokens":1}
        }}),
    ])
}

fn configure_compact(env: &PipeEnv, server: &test_utils::StubHttpServer, enabled: bool) {
    let mut config = std::fs::read_to_string(&env.config)
        .unwrap()
        .replace("http://127.0.0.1:1/v1", &server.base_url());
    if enabled {
        config.push_str("auto_compact_at = 0.8\n");
    }
    std::fs::write(&env.config, config).unwrap();
}

#[tokio::test]
async fn prelude_changes_are_checkpointed_between_cli_tool_rounds() {
    let env = pipe_env("prelude-updates");
    let server = test_utils::StubHttpServer::sequence(vec![
        model_tool(
            "prelude",
            serde_json::json!({"action": "add", "text": "new build command"}),
            100,
        ),
        model_answer("recorded", 100),
        model_answer("continued", 100),
    ])
    .await;
    configure_compact(&env, &server, false);
    let stdout = run_myco(&env, &[], b"record a fact\n/effort high\ncontinue\n/quit\n").await;
    assert!(stdout.contains("continued"), "{stdout}");
    assert_eq!(server.connections(), 3, "{stdout}");
    let saved = only_session(&env.dir);
    let thread = &saved["threads"][0];
    let messages = thread["messages"].as_array().unwrap();
    let results = &messages[2]["ToolResults"]["tool_use_results"];
    assert!(
        results.to_string().contains("[myco: Prelude changes]"),
        "{results}"
    );
    assert!(results.to_string().contains("- added:"), "{results}");
    assert_eq!(
        serde_json::to_string(messages)
            .unwrap()
            .matches("Prelude changes")
            .count(),
        1
    );
    assert_eq!(thread["user_turn_timestamps"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn automatic_compaction_resumes_once_without_inventing_user_input() {
    let env = pipe_env("auto-resume");
    let session = compact_test_session(&env);
    let server = test_utils::StubHttpServer::sequence(vec![
        model_answer("working", 80_000),
        write_summary_response(&session),
        model_answer("summary ready", 100),
        // A large retained tail must not cause a compact/resume loop.
        model_answer("continued task", 80_000),
    ])
    .await;
    configure_compact(&env, &server, true);

    let stdout = run_myco(&env, &["--resume", &session.id], b"finish task\n/quit\n").await;
    assert!(stdout.contains("continued task"), "{stdout}");
    assert_eq!(server.connections(), 4, "{stdout}");
    let request = server.captured().await.body.to_string();
    assert!(request.contains("80%"), "{request}");
    assert!(request.contains("/compact"), "{request}");

    let saved = session_json(&env.dir, &session.id);
    let threads = saved["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    let active = threads.last().unwrap();
    let messages = active["messages"].as_array().unwrap();
    let resume = messages[messages.len() - 2].to_string();
    assert!(resume.contains("# Resumption"), "{resume}");
    assert_eq!(active["user_turn_timestamps"].as_object().unwrap().len(), 1);
    assert_eq!(threads[0]["messages"].as_array().unwrap().len(), 2);
    let history = env
        .dir
        .join("profiles/default/session")
        .join(&session.id[..2])
        .join(format!("{}.history", session.id));
    let history = std::fs::read_to_string(history).unwrap();
    assert!(!history.contains("# Resumption"), "{history}");
    let replay = run_myco(&env, &["--resume", &session.id], b"/quit\n").await;
    assert!(replay.contains("finish task"), "{replay}");
    assert!(replay.contains("continued task"), "{replay}");
    assert!(!replay.contains("# Resumption"), "{replay}");
    assert!(!replay.contains("# Compaction resume"), "{replay}");
}

#[tokio::test]
async fn failing_process_status_remains_visible_when_the_model_calls_it_successful() {
    let env = pipe_env("tool-outcome");
    let server = test_utils::StubHttpServer::sequence(vec![
        model_tool("bash", serde_json::json!({"command":"exit 7"}), 100),
        model_answer("Everything succeeded.", 100),
    ])
    .await;
    configure_compact(&env, &server, false);
    let stdout = run_myco(&env, &[], b"run task\n/quit\n").await;
    assert!(stdout.contains("↳ bash exit 7: exit 7"), "{stdout}");
    assert!(stdout.contains("Everything succeeded."));
    let replay = run_myco(
        &env,
        &["--resume", &announced_session_id(&stdout)],
        b"/quit\n",
    )
    .await;
    assert!(replay.contains("↳ bash exit 7: exit 7"), "{replay}");
}

#[tokio::test]
async fn model_switch_preserves_the_shell_and_rejects_unavailable_selections() {
    let env = pipe_env("model-switch");
    let artifact = env.dir.join("kept-shell");
    let first = test_utils::StubHttpServer::sequence(vec![
        model_tool("bash", serde_json::json!({"action":"start", "session_id":"kept", "command":"bash --noprofile --norc", "idle_ms":10}), 100),
        model_answer("Shell ready.", 100),
    ]).await;
    let second = test_utils::StubHttpServer::sequence(vec![
        model_tool("bash", serde_json::json!({"action":"write", "session_id":"kept", "stdin":format!("printf retained > '{}'; echo done\n", artifact.display()), "idle_ms":10}), 100),
        model_answer("Still the same shell.", 100),
    ]).await;
    configure_compact(&env, &first, false);
    let config = std::fs::read_to_string(&env.config).unwrap();
    std::fs::write(&env.config, format!("{config}\n[models.second]\nprotocol = \"openai-responses\"\nbase_url = {:?}\nauth = {{ source = \"none\" }}\ncontext_window = 300000\n\n[models.unavailable]\nprotocol = \"openai-responses\"\nbase_url = {:?}\nauth = {{ source = \"file\", path = {:?} }}\ncontext_window = 100000\n", second.base_url(), second.base_url(), env.dir.join("missing-key"))).unwrap();
    let stdout = run_myco(&env, &[], b"start shell\n/model missing\n/model unavailable\n/model\n/model second\nuse shell\n/quit\n").await;
    assert!(stdout.contains("unknown model"), "{stdout}");
    assert!(stdout.contains("missing-key"), "{stdout}");
    assert!(stdout.contains("model=pipetest"), "{stdout}");
    assert!(stdout.contains("model=second"), "{stdout}");
    assert!(stdout.contains("/300k"), "{stdout}");
    assert_eq!(std::fs::read_to_string(artifact).unwrap(), "retained");
    assert_eq!(first.connections(), 2);
    assert_eq!(second.connections(), 2);
    let request = second.captured().await;
    assert_eq!(request.body["model"], "second");
    let session = session_json(&env.dir, &announced_session_id(&stdout));
    assert!(session.to_string().contains("previous_model"));
}

#[tokio::test]
async fn image_attachments_and_tool_results_save_references_but_upload_data() {
    let env = pipe_env("image-sidecars");
    let path = env.dir.join("image.png");
    std::fs::write(&path, [0x89, 0x50, 0x4e, 0x47]).unwrap();
    let server = test_utils::StubHttpServer::sequence(vec![
        model_tool("view_image", serde_json::json!({"path":path}), 100),
        model_answer("Seen.", 100),
    ])
    .await;
    configure_compact(&env, &server, false);
    let stdout = run_myco(
        &env,
        &[],
        format!("inspect @{}\n/quit\n", path.display()).as_bytes(),
    )
    .await;
    let saved = session_json(&env.dir, &announced_session_id(&stdout)).to_string();
    assert_eq!(saved.matches("myco-image:sha256:").count(), 2);
    assert!(!saved.contains("data:image"));
    let request = server.captured().await.body.to_string();
    assert!(request.contains("data:image/png;base64,"), "{request}");
    assert!(!request.contains("myco-image:"));
    let store = env.dir.join("profiles/default/images");
    let shards: Vec<_> = std::fs::read_dir(store).unwrap().collect();
    assert_eq!(shards.len(), 1);
    assert_eq!(
        std::fs::read_dir(shards[0].as_ref().unwrap().path())
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn print_mode_compacts_and_continues_the_same_session() {
    let env = pipe_env("print-auto");
    let session = compact_test_session(&env);
    let server = test_utils::StubHttpServer::sequence(vec![
        model_answer("working", 80_000),
        write_summary_response(&session),
        model_answer("summary ready", 100),
        model_answer("finished autonomously", 100),
    ])
    .await;
    configure_compact(&env, &server, true);
    let stdout = run_myco(&env, &["--resume", &session.id, "-p", "task"], b"").await;
    assert!(stdout.contains("finished autonomously"), "{stdout}");
    assert!(!stdout.contains("# Resumption"), "{stdout}");
    assert_eq!(server.connections(), 4);
    let saved = session_json(&env.dir, &session.id);
    assert_eq!(saved["threads"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn compaction_uses_the_configured_request_budget_and_preserves_failed_threads() {
    for limit in [None, Some(2)] {
        let env = pipe_env("compact-budget");
        let session = compact_test_session(&env);
        let mut responses = vec![model_answer("done", 100)];
        responses.extend((0..13).map(|_| model_tool("session_history", serde_json::json!({
            "action": "stats", "session_id": session.id, "thread_id": session.active_thread().id,
        }), 100)));
        responses.push(write_summary_response(&session));
        responses.push(model_answer("summary ready", 100));
        let server = test_utils::StubHttpServer::sequence(responses).await;
        configure_compact(&env, &server, false);
        if let Some(limit) = limit {
            let config = std::fs::read_to_string(&env.config).unwrap();
            std::fs::write(
                &env.config,
                format!("compaction_max_requests = {limit}\n{config}"),
            )
            .unwrap();
        }
        let stdout = run_myco(&env, &["--resume", &session.id], b"task\n/compact\n/quit\n").await;
        let saved = session_json(&env.dir, &session.id);
        if limit.is_some() {
            assert!(stdout.contains("2-request limit"), "{stdout}");
            assert_eq!(server.connections(), 3);
            assert_eq!(saved["threads"].as_array().unwrap().len(), 1);
            assert_eq!(saved["threads"][0]["id"], session.active_thread().id);
        } else {
            assert!(stdout.contains("COMPACTED"), "{stdout}");
            assert_eq!(server.connections(), 16);
            assert_eq!(saved["threads"].as_array().unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn manual_compaction_does_not_resume_automatically() {
    let env = pipe_env("manual-compact");
    let session = compact_test_session(&env);
    let server = test_utils::StubHttpServer::sequence(vec![
        model_answer("done", 100),
        write_summary_response(&session),
        model_answer("summary ready", 100),
    ])
    .await;
    configure_compact(&env, &server, true);
    let stdout = run_myco(&env, &["--resume", &session.id], b"task\n/compact\n/quit\n").await;
    assert_eq!(server.connections(), 3, "{stdout}");
    let saved = session_json(&env.dir, &session.id);
    assert_eq!(saved["threads"].as_array().unwrap().len(), 2);
    assert!(!saved.to_string().contains("# Resumption"));
}

#[tokio::test]
async fn failed_auto_compaction_keeps_the_thread_and_does_not_resume() {
    let env = pipe_env("failed-compact");
    let server = test_utils::StubHttpServer::sequence(vec![
        model_answer("done", 80_000),
        model_answer("no summary written", 100),
    ])
    .await;
    configure_compact(&env, &server, true);
    let stdout = run_myco(&env, &[], b"task\n/quit\n").await;
    assert!(stdout.contains("auto-compaction failed"), "{stdout}");
    assert_eq!(server.connections(), 2);
    let saved = session_json(&env.dir, &announced_session_id(&stdout));
    assert_eq!(saved["threads"].as_array().unwrap().len(), 1);
    assert!(!saved.to_string().contains("# Resumption"));
}

#[tokio::test]
async fn disabling_auto_compaction_applies_to_interactive_and_print_modes() {
    for print in [false, true] {
        let env = pipe_env("no-auto-compact");
        let server = test_utils::StubHttpServer::sequence(vec![model_answer("done", 80_000)]).await;
        configure_compact(&env, &server, false);
        let args = if print { vec!["-p", "task"] } else { vec![] };
        let stdout = run_myco(&env, &args, b"task\n/quit\n").await;
        assert_eq!(server.connections(), 1, "{stdout}");
        let request = server.captured().await.body.to_string();
        assert!(!request.contains("# Automatic compaction"), "{request}");
    }
}

#[tokio::test]
async fn failed_generation_does_not_start_compaction() {
    let env = pipe_env("failed-turn");
    let server =
        test_utils::StubHttpServer::sequence(vec![test_utils::StubHttpServer::status_response(
            400,
            r#"{"error":{"message":"bad request"}}"#,
        )])
        .await;
    configure_compact(&env, &server, true);
    let stdout = run_myco(&env, &[], b"task\n/quit\n").await;
    assert!(stdout.contains("ERROR"), "{stdout}");
    assert!(!stdout.contains("auto-compacting"), "{stdout}");
    assert_eq!(server.connections(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_during_a_turn_or_compaction_never_resumes() {
    for during_compaction in [false, true] {
        let env = pipe_env("cancel-compact");
        let marker = env.dir.join("tool-started");
        let mut responses = vec![];
        let server = if during_compaction {
            test_utils::StubHttpServer::sequence_then_pending(vec![model_answer("working", 80_000)])
                .await
        } else {
            responses.push(model_tool(
                "bash",
                serde_json::json!({
                    "command":format!("touch '{}' && sleep 30", marker.display()),
                }),
                80_000,
            ));
            test_utils::StubHttpServer::sequence(responses).await
        };
        configure_compact(&env, &server, true);
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .env("MYCO_HOME", &env.dir)
            .env("MYCO_PROFILE", "default")
            .env("MYCO_CONFIG", &env.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"task\n/quit\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while if during_compaction {
                server.connections() < 2
            } else {
                !marker.exists()
            } {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("work must start before cancellation");
        let status = tokio::process::Command::new("kill")
            .args(["-INT", &child.id().unwrap().to_string()])
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .expect("cancelled REPL must exit promptly")
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(!stdout.contains("resuming after"), "{stdout}");
        assert_eq!(server.connections(), if during_compaction { 2 } else { 1 });
        let saved = session_json(&env.dir, &announced_session_id(&stdout));
        assert_eq!(saved["threads"].as_array().unwrap().len(), 1);
        assert!(!saved.to_string().contains("# Resumption"));
    }
}
