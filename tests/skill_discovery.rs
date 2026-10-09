//! Real host workers discover only host-local metadata and scope notices to context.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use myco::core::ToolResource;
use myco::generative_model::{Content, ToolResult, ToolUse};
use myco::host::protocol::{HOST_PROTOCOL_VERSION, Request, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

const BODY: &str = "INSTRUCTIONS_READ_ONLY_AFTER_EXPLICIT_EDITOR_VIEW";
const NOTICE: &str = "[myco: Skills for directory";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("myco-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("launch/.git")).unwrap();
        write_skill(&root.join("launch"), ".agents", "launch", "launch metadata");
        write_skill(
            &root.join("launch/work"),
            ".grok",
            "project",
            "project metadata",
        );
        write_skill(&root.join("home"), ".claude", "personal", "home metadata");
        write_skill(
            &root.join("outside"),
            ".agents",
            "outside",
            "outside metadata",
        );
        std::fs::write(root.join("launch/work/file.txt"), "actual file content\n").unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn path(&self, path: &str) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_skill(root: &Path, layout: &str, name: &str, description: &str) -> PathBuf {
    let manifest = root.join(layout).join("skills").join(name).join("SKILL.md");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest,
        format!("---\nname: {name}\ndescription: {description}\n---\n{BODY}\n"),
    )
    .unwrap();
    manifest
}

struct Host {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    sequence: usize,
}

impl Host {
    async fn start(fixture: &Fixture) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .args(["--mode", "host", "--name", "skill-fixture"])
            .current_dir(fixture.path("launch"))
            .env("HOME", fixture.path("home"))
            .env("MYCO_HOME", fixture.path("myco"))
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut host = Self {
            input: child.stdin.take(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            sequence: 0,
        };
        match host.request(Request::Hello).await {
            Response::HelloOk { version, protocol } => {
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
                assert_eq!(protocol, 5);
                assert_eq!(protocol, HOST_PROTOCOL_VERSION);
            }
            other => panic!("unexpected handshake: {other:?}"),
        }
        host
    }

    async fn send(&mut self, request: Request) {
        let input = self.input.as_mut().unwrap();
        input.write_all(&request.encode().unwrap()).await.unwrap();
        input.flush().await.unwrap();
    }

    async fn request(&mut self, request: Request) -> Response {
        self.send(request).await;
        let mut line = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(10), self.output.read_line(&mut line))
            .await
            .expect("host discovery request hung")
            .unwrap();
        assert!(bytes > 0, "host exited before responding");
        Response::decode(&line).unwrap()
    }

    async fn call(
        &mut self,
        owner: uuid::Uuid,
        thread: &str,
        name: &str,
        input: Value,
    ) -> ToolResult {
        self.sequence += 1;
        let id = self.sequence.to_string();
        let response = self
            .request(Request::ToolCall {
                id: id.clone(),
                agent_id: owner,
                max_image_base64_bytes: None,
                thread_id: Some(thread.into()),
                tool_use: ToolUse {
                    name: name.into(),
                    input,
                },
            })
            .await;
        match response {
            Response::ToolResult {
                id: received,
                result,
            } => {
                assert_eq!(received, id);
                result
            }
            other => panic!("unexpected tool response: {other:?}"),
        }
    }

    async fn view(&mut self, owner: uuid::Uuid, thread: &str) -> String {
        let result = self
            .call(
                owner,
                thread,
                "str_replace_based_edit_tool",
                json!({
                    "command":"view", "path":"work/file.txt"
                }),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let text = text(&result);
        assert!(text.contains("actual file content"), "{text}");
        assert!(
            !text.contains(BODY),
            "instructions leaked into metadata: {text}"
        );
        text
    }

    async fn resources(&mut self, owner: uuid::Uuid) -> Vec<ToolResource> {
        match self
            .request(Request::Resources {
                id: "inventory".into(),
                agent_id: owner,
            })
            .await
        {
            Response::Resources { resources, .. } => resources,
            other => panic!("unexpected resource response: {other:?}"),
        }
    }

    async fn finish(mut self) {
        drop(self.input.take());
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .expect("host did not exit on EOF")
                .unwrap()
                .success()
        );
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn successful_file_calls_announce_metadata_changes_once_per_owner_and_thread() {
    let fixture = Fixture::new();
    let mut host = Host::start(&fixture).await;
    let owner = uuid::Uuid::new_v4();
    let first = host.view(owner, "predecessor").await;
    for expected in [
        NOTICE,
        "skill-fixture",
        "project metadata",
        "launch metadata",
        "home metadata",
    ] {
        assert!(first.contains(expected), "missing {expected}: {first}");
    }
    assert!(!first.contains("outside metadata"), "{first}");
    assert!(!host.view(owner, "predecessor").await.contains(NOTICE));

    let manifest = write_skill(
        &fixture.path("launch/work"),
        ".grok",
        "project",
        "revised metadata",
    );
    let changed = host.view(owner, "predecessor").await;
    assert!(changed.contains("revised metadata"), "{changed}");
    assert!(!changed.contains("project metadata"), "{changed}");
    std::fs::remove_dir_all(manifest.parent().unwrap()).unwrap();
    let removed = host.view(owner, "predecessor").await;
    assert!(
        removed.contains(NOTICE),
        "catalog deletion was silent: {removed}"
    );
    assert!(!removed.contains("revised metadata"), "{removed}");
    assert!(!host.view(owner, "predecessor").await.contains(NOTICE));

    assert!(
        host.view(owner, "successor")
            .await
            .contains("launch metadata")
    );
    assert!(!host.view(owner, "successor").await.contains(NOTICE));
    assert!(
        host.view(uuid::Uuid::new_v4(), "successor")
            .await
            .contains("launch metadata")
    );
    let resources = host.resources(owner).await;
    assert_eq!(resources.len(), 1, "{resources:?}");
    assert!(
        resources
            .iter()
            .all(|resource| resource.tool == "str_replace_based_edit_tool"),
        "{resources:?}"
    );

    host.send(Request::AgentFinished { agent_id: owner }).await;
    // AgentFinished has no acknowledgement. Wait for the editor's observable
    // cleanup before checking that the same owner gets a fresh skill catalog.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host.resources(owner).await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owner resources were not released");
    assert!(
        host.view(owner, "successor")
            .await
            .contains("launch metadata")
    );
    host.finish().await;
}

#[tokio::test]
async fn explicit_discovery_uses_the_worker_directory_and_home_without_loading_instructions() {
    let fixture = Fixture::new();
    let mut host = Host::start(&fixture).await;
    let owner = uuid::Uuid::new_v4();
    for _ in 0..2 {
        let result = host.call(owner, "thread", "skills", json!({})).await;
        assert!(!result.is_error, "{result:?}");
        let text = text(&result);
        assert!(
            text.contains("launch metadata") && text.contains("home metadata"),
            "{text}"
        );
        assert!(
            !text.contains("project metadata") && !text.contains(BODY),
            "{text}"
        );
    }
    assert!(
        host.resources(owner).await.is_empty(),
        "discovery created a retained resource"
    );
    let relative = host
        .call(owner, "thread", "skills", json!({"path":"work"}))
        .await;
    assert!(!relative.is_error, "{relative:?}");
    let relative = text(&relative);
    assert!(relative.contains("project metadata"), "{relative}");
    assert!(
        relative.contains(
            fixture
                .path("launch/work/.grok/skills/project/SKILL.md")
                .to_str()
                .unwrap()
        ),
        "{relative}"
    );
    assert!(!relative.contains(BODY), "{relative}");

    let outside = host
        .call(
            owner,
            "thread",
            "skills",
            json!({"path":fixture.path("outside")}),
        )
        .await;
    assert!(!outside.is_error, "{outside:?}");
    let outside = text(&outside);
    assert!(
        outside.contains("outside metadata") && outside.contains("home metadata"),
        "{outside}"
    );
    assert!(
        !outside.contains("launch metadata") && !outside.contains(BODY),
        "{outside}"
    );
    let read = host
        .call(
            owner,
            "thread",
            "str_replace_based_edit_tool",
            json!({
                "command":"view", "path":"work/.grok/skills/project/SKILL.md"
            }),
        )
        .await;
    assert!(!read.is_error, "{read:?}");
    assert!(
        text(&read).contains(BODY),
        "explicit instruction read failed: {read:?}"
    );
    host.finish().await;
}

#[tokio::test]
async fn bash_uses_launch_scope_while_successful_images_discover_their_operated_directory() {
    let fixture = Fixture::new();
    let mut host = Host::start(&fixture).await;
    let owner = uuid::Uuid::new_v4();
    let failed = host
        .call(
            owner,
            "thread",
            "str_replace_based_edit_tool",
            json!({
                "command":"view", "path":"work/missing.txt"
            }),
        )
        .await;
    assert!(failed.is_error, "{failed:?}");
    assert!(!text(&failed).contains(NOTICE), "{failed:?}");
    let bash = host
        .call(
            owner,
            "thread",
            "bash",
            json!({
                "action":"exec", "command":"cd work && printf 'changed shell directory'"
            }),
        )
        .await;
    assert!(!bash.is_error, "{bash:?}");
    let bash = text(&bash);
    assert!(
        bash.contains("changed shell directory") && bash.contains("launch metadata"),
        "{bash}"
    );
    assert!(
        !bash.contains("project metadata") && !bash.contains(BODY),
        "{bash}"
    );

    // Image transport sniffs magic bytes without decoding pixels.
    std::fs::write(fixture.path("launch/work/image.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let image = host
        .call(
            owner,
            "thread",
            "view_image",
            json!({"path":"work/image.png"}),
        )
        .await;
    assert!(!image.is_error, "{image:?}");
    assert!(
        image
            .content
            .iter()
            .any(|part| matches!(part, Content::Image { .. })),
        "{image:?}"
    );
    assert!(text(&image).contains("project metadata"), "{image:?}");
    assert!(!text(&image).contains(BODY), "{image:?}");
    assert!(!host.view(owner, "thread").await.contains(NOTICE));
    host.finish().await;
}
