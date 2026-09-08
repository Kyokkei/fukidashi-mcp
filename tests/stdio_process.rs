use std::process::Stdio;
use std::time::Duration;

use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn executable_stdio_handshake_tools_and_eof_from_unrelated_cwd() {
    let temp = tempdir().expect("temporary unrelated cwd");
    let image_path = temp.path().join("page.png");
    image::RgbImage::new(3, 2)
        .save(&image_path)
        .expect("write test page");
    let exe = std::env::var("CARGO_BIN_EXE_fukidashi-mcp")
        .expect("Cargo should provide the built binary path");
    let mut child = Command::new(exe)
        .current_dir(temp.path())
        .args(["--models-dir", "missing-models"])
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fukidashi-mcp");
    let mut input = child.stdin.take().expect("child stdin");
    let output = child.stdout.take().expect("child stdout");
    let mut output = BufReader::new(output);

    write_request(&mut input, serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    let init = read_json(&mut output).await;
    assert_eq!(init["id"], 1);
    assert!(init["result"]["serverInfo"]["name"].is_string());
    write_request(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    write_request(
        &mut input,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    let listed = read_json(&mut output).await;
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .expect("tools list")
            .len(),
        11
    );
    write_request(&mut input, serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fukidashi_analyze_page","arguments":{"image_path":"relative.png"}}})).await;
    let invalid = read_json(&mut output).await;
    assert_eq!(invalid["result"]["isError"], true);
    write_request(&mut input, serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fukidashi_serve_editor","arguments":{"image_path":image_path}}})).await;
    let editor = read_json(&mut output).await;
    assert_eq!(editor["result"]["isError"], true);
    assert!(
        editor["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("server-owned rendered stage")
    );
    write_request(&mut input, serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"fukidashi_typeset","arguments":{"image_path":image_path,"bubbles":[]}}})).await;
    let raw_typeset = read_json(&mut output).await;
    assert_eq!(raw_typeset["result"]["isError"], true);
    assert!(
        raw_typeset["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("validated server-owned clean stage")
    );
    drop(input);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("server should shut down within timeout")
        .expect("wait for server");
    assert!(status.success(), "server exit status: {status}");
}

async fn write_request(input: &mut (impl AsyncWriteExt + Unpin), value: serde_json::Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    input.write_all(&bytes).await.unwrap();
    input.flush().await.unwrap();
}

async fn read_json(output: &mut (impl AsyncBufReadExt + Unpin)) -> serde_json::Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
        .await
        .expect("read MCP response within timeout")
        .expect("read MCP response");
    assert!(!line.is_empty(), "server closed stdout before response");
    serde_json::from_str(&line).expect("stdout is a JSON MCP frame")
}
