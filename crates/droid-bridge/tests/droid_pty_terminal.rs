use std::sync::Arc;
use std::time::Duration;

use alleycat_droid_bridge::DroidBridge;
use alleycat_droid_bridge::terminal::{
    ErrorPayload, HelloPayload, StartPayload, TERMINAL_PROTOCOL_VERSION, TerminalFrame,
    TerminalFrameKind, TerminalSize, decode_frame_payload, read_frame, resize_payload,
    write_frame,
};

#[tokio::test]
async fn droid_pty_launches_interactive_tui_with_initial_geometry() {
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .build()
        .await
        .expect("build droid bridge");
    let mut client = spawn_terminal(bridge);

    client.hello().await;
    client
        .start(StartPayload {
            cols: 101,
            rows: 37,
            cwd: None,
        })
        .await;

    let first = client
        .read_output_until("FAKE_DROID_TUI_STDERR_READY")
        .await;
    assert!(first.contains("stdin_tty=true"), "{first:?}");
    assert!(first.contains("stdout_tty=true"), "{first:?}");
    assert!(first.contains("stderr_tty=true"), "{first:?}");
    assert!(first.contains("term=xterm-256color"), "{first:?}");
    assert!(first.contains("size=37x101"), "{first:?}");
    assert!(first.contains("FAKE_DROID_TUI_STDERR_READY"), "{first:?}");
}

#[tokio::test]
async fn droid_pty_forwards_input_resize_and_large_output_as_bytes() {
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .build()
        .await
        .expect("build droid bridge");
    let mut client = spawn_terminal(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;
    let _ = client.read_output_until("FAKE_DROID_TUI").await;

    let input = b"/missions\x1b[A\t\x03bracketed\x1b[200~paste\x1b[201~\n";
    client
        .write_frame(TerminalFrameKind::Input, input.to_vec())
        .await;
    let echoed = client.read_output_until("INPUT_HEX").await;
    assert!(
        echoed.contains("2f6d697373696f6e731b5b410903627261636b657465641b5b3230307e70617374651b5b3230317e0a"),
        "input bytes were not preserved: {echoed:?}"
    );

    client
        .write_frame(
            TerminalFrameKind::Resize,
            resize_payload(TerminalSize { cols: 132, rows: 43 }).to_vec(),
        )
        .await;
    client
        .write_frame(TerminalFrameKind::Input, b"SIZE?\n".to_vec())
        .await;
    let resized = client.read_output_until("SIZE:43x132").await;
    assert!(resized.contains("SIZE:43x132"), "{resized:?}");

    client
        .write_frame(TerminalFrameKind::Input, b"BURST\n".to_vec())
        .await;
    let burst = client.read_at_least_output_bytes(16 * 1024).await;
    assert!(
        burst.windows(b"0123456789abcdef".len())
            .any(|window| window == b"0123456789abcdef"),
        "large burst was not delivered"
    );
}

#[tokio::test]
async fn droid_pty_rejects_old_jsonl_peer_without_session_start() {
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move { bridge.serve_terminal_stream(server).await });

    tokio::io::AsyncWriteExt::write_all(
        &mut client,
        br#"{"jsonrpc":"2.0","method":"initialize","params":{"token":"terminal-secret-fixture"}}"#,
    )
    .await
    .unwrap();

    let frame = read_frame(&mut client).await.expect("typed error frame");
    assert_eq!(frame.kind, TerminalFrameKind::Error);
    let error: ErrorPayload = decode_frame_payload(&frame.payload).expect("error payload");
    assert_eq!(error.code, "unsupported_peer");
    assert!(!error.message.contains("terminal-secret-fixture"));

    let result = server_task.await.unwrap();
    assert!(result.is_err(), "old peer should be rejected");
}

struct TerminalClient {
    reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

fn spawn_terminal(bridge: Arc<DroidBridge>) -> TerminalClient {
    let (client, server) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        bridge.serve_terminal_stream(server).await.unwrap();
    });
    let (reader, writer) = tokio::io::split(client);
    TerminalClient { reader, writer }
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

    async fn read_at_least_output_bytes(&mut self, min_bytes: usize) -> Vec<u8> {
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while output.len() < min_bytes {
                let frame = read_frame(&mut self.reader).await.unwrap();
                match frame.kind {
                    TerminalFrameKind::Output => output.extend(frame.payload),
                    TerminalFrameKind::Exit => panic!("terminal exited during burst"),
                    TerminalFrameKind::Error => panic!("terminal error during burst"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        output
    }
}
