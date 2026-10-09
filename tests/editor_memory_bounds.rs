//! Bounded editor output must also establish its read guard in bounded memory.

#![cfg(target_os = "linux")]

use std::io::{Seek, SeekFrom, Write};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

async fn request(
    input: &mut ChildStdin,
    output: &mut BufReader<ChildStdout>,
    value: Value,
) -> Value {
    input
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
    let mut line = String::new();
    let bytes = tokio::time::timeout(Duration::from_secs(30), output.read_line(&mut line))
        .await
        .expect("bounded editor request hung")
        .unwrap();
    assert!(bytes > 0, "host died before returning the editor result");
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn a_small_view_of_a_large_file_keeps_its_read_guard_under_a_memory_limit() {
    let home = std::env::temp_dir().join(format!("myco-editor-memory-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("large.log");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"small first line\n").unwrap();
    file.set_len(1024 * 1024 * 1024).unwrap();

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"));
    command
        .args(["--mode", "host"])
        .env("MYCO_HOME", &home)
        .env("TOKIO_WORKER_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // The limit permits the runtime and its threads but cannot fit the whole
    // sparse file. Only the child changes its address-space ceiling.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 512 * 1024 * 1024,
                rlim_max: 512 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut worker = command.spawn().unwrap();
    let mut input = worker.stdin.take().unwrap();
    let mut output = BufReader::new(worker.stdout.take().unwrap());
    assert_eq!(
        request(&mut input, &mut output, json!({"type":"hello"})).await["type"],
        "hello_ok"
    );
    let owner = uuid::Uuid::new_v4();
    let view = request(&mut input, &mut output, json!({
        "type":"tool_call", "id":"1", "agent_id":owner,
        "tool_use":{"name":"str_replace_based_edit_tool", "input":{"command":"view", "path":path, "view_range":[1,1]}}
    })).await;
    assert_eq!(view["result"]["is_error"], false, "{view}");
    assert!(view.to_string().contains("small first line"), "{view}");
    let inventory = request(
        &mut input,
        &mut output,
        json!({"type":"resources", "id":"2", "agent_id":owner}),
    )
    .await;
    assert_eq!(
        inventory["resources"].as_array().unwrap().len(),
        1,
        "successful view silently skipped its read guard: {inventory}"
    );

    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(b"x").unwrap();
    let edit = request(&mut input, &mut output, json!({
        "type":"tool_call", "id":"3", "agent_id":owner,
        "tool_use":{"name":"str_replace_based_edit_tool", "input":{"command":"str_replace", "path":path, "old_str":"small", "new_str":"edited"}}
    })).await;
    assert_eq!(edit["result"]["is_error"], true, "{edit}");
    assert!(edit.to_string().contains("modified on disk"), "{edit}");
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), worker.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(file);
    std::fs::remove_dir_all(home).unwrap();
}
