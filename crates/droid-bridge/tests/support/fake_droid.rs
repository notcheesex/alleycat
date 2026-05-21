use std::io::{self, BufRead, Read, Write};

use serde_json::{Value, json};

const FACTORY_API_VERSION: &str = "1.0.0";
const FACTORY_PROTOCOL_VERSION: &str = "1.36.0";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_none_or(|arg| arg != "exec") {
        interactive_tui();
        return;
    }

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            "droid.initialize_session" | "droid.load_session" => {
                response(
                    &mut stdout,
                    id,
                    json!({"sessionId": params.get("sessionId")}),
                );
            }
            "droid.list_tools" => {
                response(
                    &mut stdout,
                    id,
                    json!({
                        "tools": [{
                            "name": "Execute",
                            "description": "Run a shell command",
                            "inputSchema": {"type":"object"}
                        }]
                    }),
                );
            }
            "droid.add_user_message" => {
                let prompt = prompt_text(&params);
                response(&mut stdout, id, json!({}));
                scripted_turn(&mut stdout, &prompt);
            }
            "droid.interrupt_session" | "droid.rename_session" => {
                response(&mut stdout, id, json!({}));
            }
            _ => {
                error(&mut stdout, id, -32601, &format!("unknown method {method}"));
            }
        }
        let _ = stdout.flush();
    }
}

fn interactive_tui() {
    enable_raw_stdin();
    let mut stdout = io::stdout();
    let _ = writeln!(
        stdout,
        "FAKE_DROID_TUI stdin_tty={} stdout_tty={} stderr_tty={} cwd={} term={} lang={} size={}x{}",
        is_tty(0),
        is_tty(1),
        is_tty(2),
        std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "?".to_string()),
        std::env::var("TERM").unwrap_or_default(),
        std::env::var("LANG").unwrap_or_default(),
        terminal_rows(),
        terminal_cols(),
    );
    let _ = writeln!(io::stderr(), "FAKE_DROID_TUI_STDERR_READY");
    let _ = stdout.flush();

    let mut stdin = io::stdin();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let chunk = &buf[..n];
        if chunk == b"\x04" || chunk == b"quit\n" {
            break;
        }
        if chunk.windows(b"BURST\n".len()).any(|window| window == b"BURST\n") {
            for _ in 0..512 {
                let _ = stdout.write_all(b"0123456789abcdef0123456789abcdef\r\n");
            }
        }
        if chunk.windows(b"SIZE?\n".len()).any(|window| window == b"SIZE?\n") {
            let _ = writeln!(stdout, "SIZE:{}x{}", terminal_rows(), terminal_cols());
        }
        let _ = writeln!(stdout, "INPUT_HEX:{}", hex_bytes(chunk));
        let _ = stdout.flush();
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(unix)]
fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(not(unix))]
fn is_tty(_fd: i32) -> bool {
    true
}

#[cfg(unix)]
fn enable_raw_stdin() {
    unsafe {
        let mut termios = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(0, &mut termios) != 0 {
            return;
        }
        termios.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        termios.c_iflag &= !(libc::ICRNL | libc::IXON);
        termios.c_oflag &= !libc::OPOST;
        termios.c_cc[libc::VMIN] = 1;
        termios.c_cc[libc::VTIME] = 0;
        let _ = libc::tcsetattr(0, libc::TCSANOW, &termios);
    }
}

#[cfg(not(unix))]
fn enable_raw_stdin() {}

#[cfg(unix)]
fn terminal_size() -> (u16, u16) {
    unsafe {
        let mut winsize = std::mem::zeroed::<libc::winsize>();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut winsize) == 0 {
            return (winsize.ws_row, winsize.ws_col);
        }
    }
    (0, 0)
}

#[cfg(not(unix))]
fn terminal_size() -> (u16, u16) {
    (0, 0)
}

fn terminal_rows() -> u16 {
    terminal_size().0
}

fn terminal_cols() -> u16 {
    terminal_size().1
}

fn scripted_turn(stdout: &mut io::Stdout, prompt: &str) {
    notification(
        stdout,
        json!({"type":"droid_working_state_changed","newState":"working"}),
    );
    notification(
        stdout,
        json!({
            "type":"create_message",
            "message":{
                "id":"user_1",
                "role":"user",
                "content":[{"type":"text","text": prompt}]
            }
        }),
    );
    if prompt.contains("conformance-marker") || prompt.contains("cat ") {
        notification(
            stdout,
            json!({
                "type":"tool_call",
                "toolUse":{
                    "id":"tool_1",
                    "name":"Execute",
                    "input":{"command":"cat conformance-marker.txt"}
                }
            }),
        );
        notification(
            stdout,
            json!({
                "type":"tool_progress_update",
                "toolUseId":"tool_1",
                "update":{"fullOutput":"alleycat-marker"}
            }),
        );
        notification(
            stdout,
            json!({
                "type":"tool_result",
                "toolUseId":"tool_1",
                "toolName":"Execute",
                "content":"alleycat-marker\n[Process exited with code 0]",
                "isError":false
            }),
        );
        assistant(stdout, "The literal contents are alleycat-marker.");
    } else {
        assistant(stdout, "OK");
    }
    notification(
        stdout,
        json!({
            "type":"session_token_usage_changed",
            "tokenUsage":{"inputTokens":1,"outputTokens":1},
            "lastCallTokenUsage":{"inputTokens":1,"outputTokens":1}
        }),
    );
    notification(
        stdout,
        json!({"type":"session_title_updated","title":"Fake Droid"}),
    );
    notification(
        stdout,
        json!({"type":"droid_working_state_changed","newState":"idle"}),
    );
}

fn assistant(stdout: &mut io::Stdout, text: &str) {
    notification(
        stdout,
        json!({
            "type":"assistant_text_delta",
            "messageId":"assistant_1",
            "textDelta": text
        }),
    );
    notification(
        stdout,
        json!({
            "type":"assistant_text_complete",
            "messageId":"assistant_1"
        }),
    );
    notification(
        stdout,
        json!({
            "type":"create_message",
            "message":{
                "id":"assistant_1",
                "role":"assistant",
                "content":[{"type":"text","text": text}]
            }
        }),
    );
}

fn prompt_text(params: &Value) -> String {
    if let Some(message) = params.get("message") {
        if let Some(text) = message.as_str() {
            return text.to_string();
        }
        if let Some(text) = message.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            return content
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn response(stdout: &mut io::Stdout, id: Value, result: Value) {
    write_frame(
        stdout,
        json!({
            "type":"response",
            "jsonrpc":"2.0",
            "factoryApiVersion": FACTORY_API_VERSION,
            "factoryProtocolVersion": FACTORY_PROTOCOL_VERSION,
            "id": id,
            "result": result
        }),
    );
}

fn error(stdout: &mut io::Stdout, id: Value, code: i64, message: &str) {
    write_frame(
        stdout,
        json!({
            "type":"response",
            "jsonrpc":"2.0",
            "factoryApiVersion": FACTORY_API_VERSION,
            "factoryProtocolVersion": FACTORY_PROTOCOL_VERSION,
            "id": id,
            "error": {"code": code, "message": message}
        }),
    );
}

fn notification(stdout: &mut io::Stdout, notification: Value) {
    write_frame(
        stdout,
        json!({
            "type":"notification",
            "jsonrpc":"2.0",
            "factoryApiVersion": FACTORY_API_VERSION,
            "factoryProtocolVersion": FACTORY_PROTOCOL_VERSION,
            "method":"droid.session_notification",
            "params":{"notification": notification}
        }),
    );
}

fn write_frame(stdout: &mut io::Stdout, frame: Value) {
    let _ = writeln!(stdout, "{}", serde_json::to_string(&frame).unwrap());
}
