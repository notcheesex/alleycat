use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use alleycat_droid_bridge::DroidBridge;
use alleycat_droid_bridge::terminal::{
    ErrorPayload, HelloPayload, StartPayload, TERMINAL_PROTOCOL_VERSION, TerminalFrame,
    TerminalFrameKind, TerminalSize, decode_frame_payload, read_frame, resize_payload, write_frame,
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
        echoed.contains(
            "2f6d697373696f6e731b5b410903627261636b657465641b5b3230307e70617374651b5b3230317e0a"
        ),
        "input bytes were not preserved: {echoed:?}"
    );

    client
        .write_frame(
            TerminalFrameKind::Resize,
            resize_payload(TerminalSize {
                cols: 132,
                rows: 43,
            })
            .to_vec(),
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
        burst
            .windows(b"0123456789abcdef".len())
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

#[tokio::test]
async fn droid_pty_missing_binary_reports_typed_error() {
    let bridge = DroidBridge::builder()
        .agent_bin("/definitely/missing/factory-droid")
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server_task) = spawn_terminal_with_task(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;

    let error = client.read_error().await;
    assert_eq!(error.code, "spawn_failed");
    assert!(error.message.contains("Droid PTY failed to start"));
    assert!(!error.message.contains("terminal-secret-fixture"));

    let result = server_task.await.unwrap();
    assert!(result.is_err(), "missing Droid should fail terminal launch");
}

#[cfg(unix)]
#[tokio::test]
async fn droid_pty_auth_failure_surfaces_terminal_output_and_exit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wrapper = write_fake_droid_wrapper(
        temp.path(),
        "fake-droid-auth-failure",
        &[("FAKE_DROID_MODE", "auth-failure".to_string())],
    );
    let bridge = DroidBridge::builder()
        .agent_bin(wrapper)
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server_task) = spawn_terminal_with_task(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;

    let output = client.read_output_until("FAKE_DROID_AUTH_REQUIRED").await;
    assert!(output.contains("Factory auth unavailable"), "{output:?}");
    assert!(!output.contains("terminal-secret-fixture"));
    assert_eq!(client.read_exit().await, 42);
    assert!(server_task.await.unwrap().is_ok());
}

#[tokio::test]
async fn droid_pty_normal_exit_emits_single_lifecycle_completion() {
    let bridge = DroidBridge::builder()
        .agent_bin(env!("CARGO_BIN_EXE_fake-droid"))
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server_task) = spawn_terminal_with_task(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;
    let _ = client.read_output_until("FAKE_DROID_TUI").await;

    client
        .write_frame(TerminalFrameKind::Input, b"quit\n".to_vec())
        .await;
    assert_eq!(client.read_exit().await, 0);
    client.assert_no_additional_exit().await;
    assert!(server_task.await.unwrap().is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn droid_pty_repeated_close_cleans_process_group_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let child_pid_file = temp.path().join("droid.pid");
    let grandchild_pid_file = temp.path().join("grandchild.pid");
    let wrapper = write_fake_droid_wrapper(
        temp.path(),
        "fake-droid-descendant",
        &[
            ("FAKE_DROID_PID_FILE", child_pid_file.display().to_string()),
            (
                "FAKE_DROID_GRANDCHILD_PID_FILE",
                grandchild_pid_file.display().to_string(),
            ),
        ],
    );
    let bridge = DroidBridge::builder()
        .agent_bin(wrapper)
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server_task) = spawn_terminal_with_task(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;
    let _ = client.read_output_until("FAKE_DROID_TUI").await;
    let child_pid = wait_for_pid_file(&child_pid_file).await;
    let grandchild_pid = wait_for_pid_file(&grandchild_pid_file).await;

    client
        .write_frame(TerminalFrameKind::Close, Vec::new())
        .await;
    let _ = client
        .try_write_frame(TerminalFrameKind::Close, Vec::new())
        .await;
    let _ = client.read_exit().await;
    client.assert_no_additional_exit().await;
    assert!(server_task.await.unwrap().is_ok());
    wait_for_process_gone(child_pid).await;
    wait_for_process_gone(grandchild_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn droid_pty_abrupt_disconnect_forces_cleanup() {
    let temp = tempfile::tempdir().expect("tempdir");
    let child_pid_file = temp.path().join("droid.pid");
    let grandchild_pid_file = temp.path().join("grandchild.pid");
    let wrapper = write_fake_droid_wrapper(
        temp.path(),
        "fake-droid-disconnect",
        &[
            ("FAKE_DROID_PID_FILE", child_pid_file.display().to_string()),
            (
                "FAKE_DROID_GRANDCHILD_PID_FILE",
                grandchild_pid_file.display().to_string(),
            ),
        ],
    );
    let bridge = DroidBridge::builder()
        .agent_bin(wrapper)
        .build()
        .await
        .expect("build droid bridge");
    let (mut client, server_task) = spawn_terminal_with_task(bridge);
    client.hello().await;
    client
        .start(StartPayload {
            cols: 80,
            rows: 24,
            cwd: None,
        })
        .await;
    let _ = client.read_output_until("FAKE_DROID_TUI").await;
    let child_pid = wait_for_pid_file(&child_pid_file).await;
    let grandchild_pid = wait_for_pid_file(&grandchild_pid_file).await;

    drop(client);
    let result = tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .expect("server task should finish after disconnect")
        .expect("server task join");
    assert!(result.is_ok());
    wait_for_process_gone(child_pid).await;
    wait_for_process_gone(grandchild_pid).await;
}

struct TerminalClient {
    reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

fn spawn_terminal(bridge: Arc<DroidBridge>) -> TerminalClient {
    spawn_terminal_with_task(bridge).0
}

fn spawn_terminal_with_task(
    bridge: Arc<DroidBridge>,
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

    async fn write_frame(&mut self, kind: TerminalFrameKind, payload: Vec<u8>) {
        self.try_write_frame(kind, payload).await.unwrap();
    }

    async fn try_write_frame(
        &mut self,
        kind: TerminalFrameKind,
        payload: Vec<u8>,
    ) -> Result<(), alleycat_droid_bridge::terminal::TerminalWireError> {
        write_frame(&mut self.writer, &TerminalFrame { kind, payload }).await
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

    async fn read_error(&mut self) -> ErrorPayload {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = read_frame(&mut self.reader).await.unwrap();
                if frame.kind == TerminalFrameKind::Error {
                    return decode_frame_payload(&frame.payload).unwrap();
                }
            }
        })
        .await
        .unwrap()
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

    async fn assert_no_additional_exit(&mut self) {
        let mut exits = 0usize;
        let _ = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                match read_frame(&mut self.reader).await {
                    Ok(frame) if frame.kind == TerminalFrameKind::Exit => exits += 1,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .await;
        assert_eq!(exits, 0, "duplicate lifecycle completion frame");
    }
}

fn decode_exit_code(payload: &[u8]) -> i32 {
    assert_eq!(payload.len(), 4, "exit payload must be four bytes");
    i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
}

#[cfg(unix)]
fn write_fake_droid_wrapper(dir: &Path, name: &str, env: &[(&str, String)]) -> PathBuf {
    let path = dir.join(name);
    let mut script = String::from("#!/bin/sh\nset -eu\n");
    for (key, value) in env {
        script.push_str("export ");
        script.push_str(key);
        script.push('=');
        script.push_str(&shell_quote(value));
        script.push('\n');
    }
    script.push_str("exec ");
    script.push_str(&shell_quote(env!("CARGO_BIN_EXE_fake-droid")));
    script.push_str(" \"$@\"\n");
    std::fs::write(&path, script).expect("write fake droid wrapper");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
async fn wait_for_pid_file(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if let Ok(pid) = contents.trim().parse::<libc::pid_t>() {
                    return pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("pid file should be written")
}

#[cfg(unix)]
async fn wait_for_process_gone(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while process_exists(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} should be gone"));
}

#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
