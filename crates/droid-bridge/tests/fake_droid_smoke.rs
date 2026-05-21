use std::time::Duration;

use alleycat_bridge_core::framing::{read_json_line, write_json_line};
use alleycat_bridge_core::server::serve_stream;
use alleycat_bridge_core::{JsonRpcRequest, JsonRpcVersion, RequestId};
use alleycat_droid_bridge::DroidBridge;
use alleycat_droid_bridge::terminal::{
    HelloPayload, StartPayload, TERMINAL_PROTOCOL_VERSION, TerminalFrame, TerminalFrameKind,
    TerminalSize, decode_frame_payload, read_frame, resize_payload, write_frame,
};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};

#[tokio::test]
async fn initialize_thread_start_turn_start_smoke() {
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .build()
        .await
        .expect("build droid bridge");
    let (client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(serve_stream(bridge, server));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);

    send(
        &mut write,
        1,
        "initialize",
        json!({"clientInfo":{"name":"fake-droid-smoke","version":"0"}}),
    )
    .await;
    let init = read_until_response(&mut read, 1).await;
    assert_eq!(init["result"]["userAgent"], "alleycat-droid-bridge/0.1.0");

    let cwd = tempfile::TempDir::new().unwrap();
    send(
        &mut write,
        2,
        "thread/start",
        json!({"cwd": cwd.path().to_string_lossy()}),
    )
    .await;
    let start = read_until_response(&mut read, 2).await;
    let thread_id = start["result"]["thread"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(start["result"]["modelProvider"], "droid");

    send(
        &mut write,
        3,
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [{"type":"text","text":"Reply with exactly OK."}]
        }),
    )
    .await;
    let turn = read_until_response(&mut read, 3).await;
    assert_eq!(turn["result"]["turn"]["status"], "inProgress");

    let mut saw_delta = false;
    let mut saw_completed = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !saw_completed {
            let frame: Value = read_json_line(&mut read).await.unwrap().unwrap();
            if frame.get("method").and_then(Value::as_str) == Some("item/agentMessage/delta") {
                saw_delta = frame["params"]["delta"] == "OK";
            }
            if frame.get("method").and_then(Value::as_str) == Some("turn/completed") {
                saw_completed = true;
            }
        }
    })
    .await
    .expect("turn should complete");
    assert!(saw_delta, "expected assistant delta");

    drop(write);
    server_task.abort();
}

#[tokio::test]
async fn json_native_streams_while_droid_pty_sessions_are_active_and_closed() {
    let state = tempfile::TempDir::new().expect("isolated bridge state");
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .codex_home(state.path().join("codex-home"))
        .factory_sessions_dir(state.path().join("factory-sessions"))
        .build()
        .await
        .expect("build droid bridge");

    let (mut pty_one, pty_one_task) = spawn_terminal(bridge.clone());
    pty_one.hello().await;
    pty_one
        .start(StartPayload {
            cols: 90,
            rows: 25,
            cwd: None,
        })
        .await;
    let pty_one_ready = pty_one.read_output_until("FAKE_DROID_TUI").await;
    assert!(pty_one_ready.contains("size=25x90"), "{pty_one_ready:?}");

    let (mut pty_two, pty_two_task) = spawn_terminal(bridge.clone());
    pty_two.hello().await;
    pty_two
        .start(StartPayload {
            cols: 100,
            rows: 31,
            cwd: None,
        })
        .await;
    let pty_two_ready = pty_two.read_output_until("FAKE_DROID_TUI").await;
    assert!(pty_two_ready.contains("size=31x100"), "{pty_two_ready:?}");

    pty_one.input(b"PTY_ONE_ONLY\n").await;
    let pty_one_echo = pty_one
        .read_output_until("5054595f4f4e455f4f4e4c590a")
        .await;
    assert!(
        pty_one_echo.contains("5054595f4f4e455f4f4e4c590a"),
        "PTY one input was not echoed on its own terminal: {pty_one_echo:?}"
    );
    assert!(
        !pty_one_echo.contains("5054595f54574f5f4f4e4c590a"),
        "PTY two bytes leaked into PTY one transcript: {pty_one_echo:?}"
    );
    pty_two.input(b"PTY_TWO_ONLY\n").await;
    let pty_two_echo = pty_two
        .read_output_until("5054595f54574f5f4f4e4c590a")
        .await;
    assert!(
        pty_two_echo.contains("5054595f54574f5f4f4e4c590a"),
        "PTY two input was not echoed on its own terminal: {pty_two_echo:?}"
    );
    assert!(
        !pty_two_echo.contains("5054595f4f4e455f4f4e4c590a"),
        "PTY one bytes leaked into PTY two transcript: {pty_two_echo:?}"
    );

    let mut json_native = JsonNativeClient::connect(bridge.clone());
    json_native.initialize().await;
    json_native.start_thread().await;
    let first_turn = json_native.turn("Reply with exactly OK.").await;
    assert_json_native_frames_do_not_contain_terminal_bytes(&first_turn);

    pty_one.close().await;
    let _ = pty_one.read_exit().await;
    assert!(
        pty_one_task.await.unwrap().is_ok(),
        "closing one PTY session should finish only that terminal runtime"
    );

    let second_turn = json_native
        .turn("Reply with exactly OK after PTY one closes.")
        .await;
    assert_json_native_frames_do_not_contain_terminal_bytes(&second_turn);

    pty_two
        .resize(TerminalSize {
            cols: 132,
            rows: 43,
        })
        .await;
    pty_two.input(b"SIZE?\n").await;
    let resized = pty_two.read_output_until("SIZE:43x132").await;
    assert!(
        !resized.contains("turn/completed") && !resized.contains("\"jsonrpc\""),
        "JSON/native frames leaked into PTY terminal output: {resized:?}"
    );
    assert!(
        !resized.contains("5054595f4f4e455f4f4e4c590a"),
        "closed PTY one's bytes leaked into PTY two transcript: {resized:?}"
    );

    pty_two.close().await;
    let _ = pty_two.read_exit().await;
    assert!(
        pty_two_task.await.unwrap().is_ok(),
        "closing the second PTY session should not depend on JSON/native state"
    );
    let final_turn = json_native
        .turn("Reply with exactly OK after both PTY sessions close.")
        .await;
    assert_json_native_frames_do_not_contain_terminal_bytes(&final_turn);
    json_native.close().await;
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    id: i64,
    method: &str,
    params: Value,
) {
    write_json_line(
        writer,
        &JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Integer(id),
            method: method.to_string(),
            params: Some(params),
        },
    )
    .await
    .unwrap();
}

async fn read_until_response<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, id: i64) -> Value {
    read_until_response_collecting(reader, id).await.0
}

async fn read_until_response_collecting<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    id: i64,
) -> (Value, Vec<Value>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut skipped = Vec::new();
        loop {
            let value: Value = read_json_line(reader).await.unwrap().unwrap();
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                return (value, skipped);
            }
            skipped.push(value);
        }
    })
    .await
    .unwrap()
}

struct JsonNativeClient {
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    thread_id: Option<String>,
    cwd: Option<tempfile::TempDir>,
    server_task: tokio::task::JoinHandle<anyhow::Result<()>>,
    next_id: i64,
}

impl JsonNativeClient {
    fn connect(bridge: std::sync::Arc<DroidBridge>) -> Self {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(serve_stream(bridge, server));
        let (read, writer) = tokio::io::split(client);
        Self {
            reader: BufReader::new(read),
            writer,
            thread_id: None,
            cwd: None,
            server_task,
            next_id: 1,
        }
    }

    async fn initialize(&mut self) {
        let id = self.next_request_id();
        send(
            &mut self.writer,
            id,
            "initialize",
            json!({"clientInfo":{"name":"fake-droid-json-native-regression","version":"0"}}),
        )
        .await;
        let init = read_until_response(&mut self.reader, id).await;
        assert_eq!(init["result"]["userAgent"], "alleycat-droid-bridge/0.1.0");
    }

    async fn start_thread(&mut self) {
        let cwd = tempfile::TempDir::new().unwrap();
        let id = self.next_request_id();
        send(
            &mut self.writer,
            id,
            "thread/start",
            json!({"cwd": cwd.path().to_string_lossy()}),
        )
        .await;
        let start = read_until_response(&mut self.reader, id).await;
        assert_eq!(start["result"]["modelProvider"], "droid");
        self.thread_id = Some(
            start["result"]["thread"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
        self.cwd = Some(cwd);
    }

    async fn turn(&mut self, prompt: &str) -> Vec<Value> {
        let thread_id = self
            .thread_id
            .as_ref()
            .expect("thread must be started before turn")
            .clone();
        let id = self.next_request_id();
        send(
            &mut self.writer,
            id,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type":"text","text": prompt}]
            }),
        )
        .await;
        let (turn, mut frames) = read_until_response_collecting(&mut self.reader, id).await;
        assert_eq!(turn["result"]["turn"]["status"], "inProgress");

        let mut saw_delta = frames.iter().any(is_ok_delta);
        let mut saw_completed = frames.iter().any(is_turn_completed);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !saw_completed {
                let frame: Value = read_json_line(&mut self.reader).await.unwrap().unwrap();
                saw_delta |= is_ok_delta(&frame);
                saw_completed |= is_turn_completed(&frame);
                frames.push(frame);
            }
        })
        .await
        .expect("turn should complete");
        assert!(saw_delta, "expected assistant delta");
        frames
    }

    async fn close(self) {
        let JsonNativeClient {
            reader,
            mut writer,
            server_task,
            cwd,
            thread_id: _,
            next_id: _,
        } = self;
        writer
            .shutdown()
            .await
            .expect("shutdown JSON/native client writer");
        drop(writer);
        drop(reader);
        let result = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("JSON/native serve_stream should stop after client shutdown")
            .expect("JSON/native serve_stream task should join");
        assert!(
            result.is_ok(),
            "JSON/native serve_stream returned an error during normal shutdown: {result:?}"
        );
        drop(cwd);
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

struct TerminalClient {
    reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

fn spawn_terminal(
    bridge: std::sync::Arc<DroidBridge>,
) -> (TerminalClient, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (client, server) = tokio::io::duplex(256 * 1024);
    let server_task = tokio::spawn(async move { bridge.serve_terminal_stream(server).await });
    let (reader, writer) = tokio::io::split(client);
    (TerminalClient { reader, writer }, server_task)
}

impl TerminalClient {
    async fn hello(&mut self) {
        self.write_frame(
            TerminalFrameKind::Hello,
            serde_json::to_vec(&HelloPayload {
                min_version: TERMINAL_PROTOCOL_VERSION,
                max_version: TERMINAL_PROTOCOL_VERSION,
                features: vec![
                    "output".to_string(),
                    "input".to_string(),
                    "resize".to_string(),
                    "close".to_string(),
                    "error".to_string(),
                ],
            })
            .unwrap(),
        )
        .await;
        let hello = read_frame(&mut self.reader).await.unwrap();
        assert_eq!(hello.kind, TerminalFrameKind::Hello);
        let payload: HelloPayload = decode_frame_payload(&hello.payload).unwrap();
        assert_eq!(payload.max_version, TERMINAL_PROTOCOL_VERSION);
    }

    async fn start(&mut self, payload: StartPayload) {
        self.write_frame(
            TerminalFrameKind::Start,
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
    }

    async fn input(&mut self, input: &[u8]) {
        self.write_frame(TerminalFrameKind::Input, input.to_vec())
            .await;
    }

    async fn resize(&mut self, size: TerminalSize) {
        self.write_frame(TerminalFrameKind::Resize, resize_payload(size).to_vec())
            .await;
    }

    async fn close(&mut self) {
        self.write_frame(TerminalFrameKind::Close, Vec::new())
            .await;
    }

    async fn write_frame(&mut self, kind: TerminalFrameKind, payload: Vec<u8>) {
        write_frame(&mut self.writer, &TerminalFrame { kind, payload })
            .await
            .unwrap();
    }

    async fn read_output_until(&mut self, needle: &str) -> String {
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = read_frame(&mut self.reader).await.unwrap();
                match frame.kind {
                    TerminalFrameKind::Output => output.extend(frame.payload),
                    TerminalFrameKind::Exit => panic!("terminal exited before {needle}"),
                    TerminalFrameKind::Error => panic!("terminal error before {needle}"),
                    _ => {}
                }
                if String::from_utf8_lossy(&output).contains(needle) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        String::from_utf8_lossy(&output).to_string()
    }

    async fn read_exit(&mut self) -> i32 {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = read_frame(&mut self.reader).await.unwrap();
                match frame.kind {
                    TerminalFrameKind::Exit => return decode_exit_code(&frame.payload),
                    TerminalFrameKind::Error => panic!("terminal error before exit"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }
}

fn assert_json_native_frames_do_not_contain_terminal_bytes(frames: &[Value]) {
    let transcript = frames
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "FAKE_DROID_TUI",
        "INPUT_HEX",
        "PTY_ONE_ONLY",
        "PTY_TWO_ONLY",
        "5054595f4f4e455f4f4e4c590a",
        "5054595f54574f5f4f4e4c590a",
    ] {
        assert!(
            !transcript.contains(forbidden),
            "terminal bytes leaked into JSON/native transcript via {forbidden}: {transcript}"
        );
    }
}

fn is_ok_delta(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("item/agentMessage/delta")
        && frame["params"]["delta"] == "OK"
}

fn is_turn_completed(frame: &Value) -> bool {
    frame.get("method").and_then(Value::as_str) == Some("turn/completed")
}

fn decode_exit_code(payload: &[u8]) -> i32 {
    assert_eq!(payload.len(), 4, "exit payload must be four bytes");
    i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
}
