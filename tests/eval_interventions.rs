//! Reporting joins observer assertions to immutable fixture results; no model calls.

use std::path::PathBuf;
use std::time::Duration;

use myco::core::image_store::sha256;
use serde_json::{Value, json};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("myco-interventions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("runs")).unwrap();
        Self { root }
    }

    fn result(&self, name: &str, model: &str) -> PathBuf {
        let path = self.root.join("runs").join(name).join("result.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let result = json!({"version":1,"fingerprint":sha256(name.as_bytes()),"case_hash":"case-hash","case":"fixture","split":"test","model":model,"model_hash":model,"cohort_hash":"cohort","free_only":true,"repetition":0,"prelude_hash":"prelude","myco_version":"fixture","myco_commit":"fixture","status":"cancelled","score":null,"feedback":"synthetic result","agent":{
            "status":"cancelled","error":null,"answer":"","session_id":"fixture","elapsed_ms":1,"metrics":{
                "requests":0,"compaction_requests":0,"requests_with_usage":0,"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"tool_calls":1,"tool_errors":1,"compactions":0,"request_limit_reached":false
            }
        }});
        std::fs::write(&path, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
        path
    }

    fn annotation(&self, name: &str, count: u64) -> Value {
        let bytes = std::fs::read(self.root.join("runs").join(name).join("result.json")).unwrap();
        let result: Value = serde_json::from_slice(&bytes).unwrap();
        json!({"fingerprint":result["fingerprint"],"result_sha256":sha256(&bytes),"human_interventions":count,"observer":"synthetic test observer","annotated_at":"2026-10-06T12:00:00Z","note":"Fixture assertion, not a human benchmark.","evidence":["notes-not-present.md#observation"]})
    }

    fn annotations(&self, entries: Vec<Value>) {
        self.document(json!({"version":1,"annotations":entries}));
    }

    fn document(&self, value: Value) {
        std::fs::write(
            self.root.join("annotations.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    async fn cli(&self, annotated: bool) -> std::process::Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco-eval"));
        command
            .arg("report")
            .arg(self.root.join("runs"))
            .kill_on_drop(true);
        if annotated {
            command
                .arg("--interventions")
                .arg(self.root.join("annotations.json"));
        }
        tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap()
    }

    async fn report(&self, annotated: bool) -> Value {
        let output = self.cli(annotated).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    async fn rejects(&self, expected: &str) {
        let output = self.cli(true).await;
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{output:?}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn observer_counts_require_full_group_coverage_and_never_follow_cancelled_status() {
    let fixture = Fixture::new();
    fixture.result("one", "model");
    fixture.result("two", "model");
    fixture.result("other", "other-model");
    let unannotated = fixture.report(false).await;
    assert!(unannotated["intervention_annotations"].is_null());
    assert_eq!(unannotated["groups"][0]["tool_errors"], 2);
    assert!(unannotated["groups"][0]["observer_reported_human_interventions"].is_null());
    assert_eq!(unannotated["groups"][0]["intervention_unannotated_runs"], 2);
    fixture.annotations(vec![
        fixture.annotation("one", 0),
        fixture.annotation("other", 0),
    ]);
    let partial = fixture.report(true).await;
    assert!(partial["groups"][0]["observer_reported_human_interventions"].is_null());
    assert_eq!(partial["groups"][0]["intervention_annotated_runs"], 1);
    assert_eq!(partial["groups"][0]["intervention_unannotated_runs"], 1);
    assert_eq!(
        partial["groups"][1]["observer_reported_human_interventions"],
        0
    );
    fixture.annotations(vec![
        fixture.annotation("one", 0),
        fixture.annotation("two", 3),
        fixture.annotation("other", 0),
    ]);
    let complete = fixture.report(true).await;
    assert_eq!(
        complete["groups"][0]["observer_reported_human_interventions"],
        3
    );
    assert_eq!(complete["groups"][0]["intervention_annotated_runs"], 2);
    assert_eq!(complete["groups"][0]["tool_errors"], 2);
    assert_eq!(
        complete["groups"][1]["observer_reported_human_interventions"],
        0
    );
}

#[tokio::test]
async fn moved_bundles_keep_annotations_and_reporting_leaves_original_artifacts_unchanged() {
    let mut fixture = Fixture::new();
    let result = fixture.result("one", "model");
    let unknown = fixture.report(false).await;
    assert!(unknown["groups"][0]["observer_reported_human_interventions"].is_null());
    assert_eq!(unknown["groups"][0]["intervention_unannotated_runs"], 1);
    fixture.annotations(vec![fixture.annotation("one", 0)]);
    let trace = result.with_file_name("events.jsonl");
    std::fs::write(&trace, "original trace\n").unwrap();
    let paths = [
        PathBuf::from("runs/one/result.json"),
        PathBuf::from("runs/one/events.jsonl"),
        PathBuf::from("annotations.json"),
    ];
    let originals: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(fixture.root.join(path)).unwrap())
        .collect();
    let before = fixture.report(true).await;
    assert_eq!(before["groups"][0]["tool_errors"], 1);
    assert_eq!(
        before["groups"][0]["observer_reported_human_interventions"],
        0
    );
    assert_eq!(before["results"][0]["result_sha256"], sha256(&originals[0]));
    assert_eq!(
        before["intervention_annotations"]["source_sha256"],
        sha256(&originals[2])
    );
    let moved = fixture.root.with_extension("moved");
    std::fs::rename(&fixture.root, &moved).unwrap();
    fixture.root = moved;
    assert_eq!(fixture.report(true).await, before);
    for (path, bytes) in paths.iter().zip(originals) {
        assert_eq!(std::fs::read(fixture.root.join(path)).unwrap(), bytes);
    }
}

#[tokio::test]
async fn fresh_attempts_with_same_fingerprint_need_independent_result_hash_annotations() {
    let fixture = Fixture::new();
    let first = fixture.result("one", "model");
    let second = fixture.result("two", "model");
    let mut changed: Value = serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
    changed["agent"]["elapsed_ms"] = json!(2);
    std::fs::write(&second, changed.to_string()).unwrap();
    fixture.annotations(vec![fixture.annotation("one", 0)]);
    let partial = fixture.report(true).await;
    assert_eq!(
        partial["results"][0]["fingerprint"],
        partial["results"][1]["fingerprint"]
    );
    assert_ne!(
        partial["results"][0]["result_sha256"],
        partial["results"][1]["result_sha256"]
    );
    assert!(partial["groups"][0]["observer_reported_human_interventions"].is_null());
    fixture.annotations(vec![
        fixture.annotation("one", 0),
        fixture.annotation("two", 2),
    ]);
    assert_eq!(
        fixture.report(true).await["groups"][0]["observer_reported_human_interventions"],
        2
    );
    std::fs::copy(first, second).unwrap();
    fixture.rejects("ambiguous duplicate result identity").await;
    assert!(!fixture.cli(false).await.status.success());
}

#[tokio::test]
async fn stale_unknown_duplicate_and_overflowing_annotations_fail_without_partial_reports() {
    let fixture = Fixture::new();
    let first = fixture.result("one", "model");
    fixture.result("two", "model");
    let valid = fixture.annotation("one", 0);
    fixture.annotations(vec![valid.clone(), valid.clone()]);
    fixture.rejects("duplicate intervention annotation").await;
    let mut unknown = valid.clone();
    unknown["fingerprint"] = json!("f".repeat(64));
    fixture.annotations(vec![unknown]);
    fixture.rejects("unknown result").await;
    let mut stale = valid.clone();
    stale["result_sha256"] = json!("f".repeat(64));
    fixture.annotations(vec![stale]);
    fixture.rejects("stale or mismatched").await;
    fixture.annotations(vec![
        fixture.annotation("one", u64::MAX),
        fixture.annotation("two", 1),
    ]);
    fixture.rejects("overflows u64").await;
    fixture.annotations(vec![valid]);
    let original = std::fs::read(&first).unwrap();
    let mut changed = original;
    changed.push(b'\n');
    std::fs::write(first, changed).unwrap();
    fixture.rejects("stale or mismatched").await;
}

#[tokio::test]
async fn annotation_schema_and_regular_file_limits_are_enforced() {
    let fixture = Fixture::new();
    fixture.result("one", "model");
    let valid = fixture.annotation("one", 0);
    fixture.document(json!({"version":2,"annotations":[valid.clone()]}));
    fixture
        .rejects("unsupported intervention annotation version")
        .await;
    for (field, value, expected) in [
        ("human_interventions", json!(-1), "invalid value"),
        ("human_interventions", json!(0.5), "invalid type"),
        ("observer", json!(" "), "must be nonempty"),
        ("note", json!(""), "must be nonempty"),
        ("evidence", json!([""]), "must be nonempty"),
        ("annotated_at", json!("yesterday"), "parse interventions"),
        ("fingerprint", json!("short"), "64 lowercase hexadecimal"),
        ("unrecognized", json!(true), "unknown field"),
    ] {
        let mut entry = valid.clone();
        entry[field] = value;
        fixture.annotations(vec![entry]);
        fixture.rejects(expected).await;
    }
    let path = fixture.root.join("annotations.json");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(4 * 1024 * 1024 + 1)
        .unwrap();
    fixture.rejects("regular file no larger than 4 MiB").await;
    std::fs::remove_file(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        fixture.rejects("regular file").await;
    }
}
