use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, anyhow};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub const TERMINAL_PROTOCOL_VERSION: u16 = 1;
const MAGIC: &[u8; 4] = b"DPTY";
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalFrameKind {
    Hello = 1,
    Start = 2,
    Output = 3,
    Input = 4,
    Resize = 5,
    Close = 6,
    Exit = 7,
    Error = 8,
}

impl TerminalFrameKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Hello),
            2 => Some(Self::Start),
            3 => Some(Self::Output),
            4 => Some(Self::Input),
            5 => Some(Self::Resize),
            6 => Some(Self::Close),
            7 => Some(Self::Exit),
            8 => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalFrame {
    pub kind: TerminalFrameKind,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloPayload {
    pub min_version: u16,
    pub max_version: u16,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartPayload {
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalWireError {
    #[error("unsupported terminal peer: {0}")]
    UnsupportedPeer(String),
    #[error("terminal frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("unknown terminal frame kind: {0}")]
    UnknownKind(u8),
    #[error("terminal wire I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal frame payload decode failed")]
    Decode,
}

pub fn frame_payload_json<T: Serialize>(value: &T) -> Result<Vec<u8>, TerminalWireError> {
    serde_json::to_vec(value).map_err(|_| TerminalWireError::Decode)
}

pub fn decode_frame_payload<T: serde::de::DeserializeOwned>(
    payload: &[u8],
) -> Result<T, TerminalWireError> {
    serde_json::from_slice(payload).map_err(|_| TerminalWireError::Decode)
}

pub async fn read_frame<R>(reader: &mut R) -> Result<TerminalFrame, TerminalWireError>
where
    R: AsyncRead + Unpin,
{
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        return Err(TerminalWireError::UnsupportedPeer(format!(
            "invalid magic {:02x}{:02x}{:02x}{:02x}",
            magic[0], magic[1], magic[2], magic[3]
        )));
    }
    let version = reader.read_u16().await?;
    if version != TERMINAL_PROTOCOL_VERSION {
        return Err(TerminalWireError::UnsupportedPeer(format!(
            "version {version} is not supported"
        )));
    }
    let kind = reader.read_u8().await?;
    let _flags = reader.read_u8().await?;
    let len = reader.read_u32().await?;
    if len as usize > MAX_FRAME_BYTES {
        return Err(TerminalWireError::FrameTooLarge(len));
    }
    let kind = TerminalFrameKind::from_u8(kind).ok_or(TerminalWireError::UnknownKind(kind))?;
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(TerminalFrame { kind, payload })
}

pub async fn write_frame<W>(writer: &mut W, frame: &TerminalFrame) -> Result<(), TerminalWireError>
where
    W: AsyncWrite + Unpin,
{
    if frame.payload.len() > MAX_FRAME_BYTES {
        return Err(TerminalWireError::FrameTooLarge(frame.payload.len() as u32));
    }
    writer.write_all(MAGIC).await?;
    writer.write_u16(TERMINAL_PROTOCOL_VERSION).await?;
    writer.write_u8(frame.kind as u8).await?;
    writer.write_u8(0).await?;
    writer.write_u32(frame.payload.len() as u32).await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await?;
    Ok(())
}

pub fn resize_payload(size: TerminalSize) -> [u8; 4] {
    let mut out = [0u8; 4];
    out[..2].copy_from_slice(&size.cols.to_be_bytes());
    out[2..].copy_from_slice(&size.rows.to_be_bytes());
    out
}

pub fn decode_resize_payload(payload: &[u8]) -> Result<TerminalSize, TerminalWireError> {
    if payload.len() != 4 {
        return Err(TerminalWireError::Decode);
    }
    Ok(TerminalSize {
        cols: u16::from_be_bytes([payload[0], payload[1]]),
        rows: u16::from_be_bytes([payload[2], payload[3]]),
    })
}

pub fn exit_payload(code: i32) -> [u8; 4] {
    code.to_be_bytes()
}

fn decode_exit_payload(payload: &[u8]) -> Result<i32, TerminalWireError> {
    if payload.len() != 4 {
        return Err(TerminalWireError::Decode);
    }
    Ok(i32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

pub async fn serve_droid_terminal_stream<S>(droid_bin: PathBuf, stream: S) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    negotiate_terminal(&mut reader, &mut writer).await?;
    let start = read_frame(&mut reader).await?;
    if start.kind != TerminalFrameKind::Start {
        write_error(&mut writer, "expected_start", "expected start frame").await?;
        return Err(anyhow!("expected terminal start frame"));
    }
    let start: StartPayload = match decode_frame_payload(&start.payload) {
        Ok(payload) => payload,
        Err(error) => {
            write_error(&mut writer, "invalid_start", "invalid start payload").await?;
            return Err(anyhow!(error));
        }
    };
    let size = TerminalSize {
        cols: start.cols,
        rows: start.rows,
    };
    if let Err(error) = validate_size(size) {
        write_error(&mut writer, "invalid_size", &error.to_string()).await?;
        return Err(error);
    }
    let spawned = match tokio::task::spawn_blocking(move || {
        DroidTerminalSession::spawn("droid-pty".to_string(), droid_bin, start.cwd, size)
    })
    .await
    {
        Ok(Ok(spawned)) => spawned,
        Ok(Err(error)) => {
            let message = format!("Droid PTY failed to start: {error:#}");
            write_error(&mut writer, "spawn_failed", &message).await?;
            return Err(error);
        }
        Err(error) => {
            write_error(&mut writer, "spawn_failed", "Droid PTY spawn task failed").await?;
            return Err(anyhow!(error).context("joining droid PTY spawn task"));
        }
    };

    let session = spawned.session;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    spawn_output_thread(spawned.reader, event_tx.clone());
    spawn_wait_thread(spawned.child, event_tx);

    let result: anyhow::Result<()> = async {
        let mut close_requested = false;
        let mut output_closed = false;
        let mut pending_exit = None;
        'terminal: loop {
            tokio::select! {
                frame = read_frame(&mut reader), if !close_requested => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(error) => {
                            debug!(error = %error, "droid PTY client stream ended");
                            shutdown_session(Arc::clone(&session), "client_disconnect").await?;
                            break 'terminal Ok(());
                        }
                    };
                    match frame.kind {
                        TerminalFrameKind::Input => {
                            if close_requested {
                                debug!("ignoring Droid PTY input after close");
                                continue;
                            }
                            let byte_count = frame.payload.len();
                            let session = Arc::clone(&session);
                            tokio::task::spawn_blocking(move || session.write(&frame.payload))
                                .await
                                .context("joining droid PTY input task")??;
                            debug!(byte_count, "forwarded droid PTY input");
                        }
                        TerminalFrameKind::Resize => {
                            if close_requested {
                                debug!("ignoring Droid PTY resize after close");
                                continue;
                            }
                            let size = decode_resize_payload(&frame.payload)?;
                            validate_size(size)?;
                            let session = Arc::clone(&session);
                            tokio::task::spawn_blocking(move || session.resize(size))
                                .await
                                .context("joining droid PTY resize task")??;
                        }
                        TerminalFrameKind::Close => {
                            close_requested = true;
                            shutdown_session(Arc::clone(&session), "client_close").await?;
                        }
                        _ => {
                            write_error(&mut writer, "unexpected_frame", "unexpected client terminal frame").await?;
                        }
                    }
                }
                event = event_rx.recv() => {
                    match event {
                        Some(TerminalEvent::Output(bytes)) => {
                            let byte_count = bytes.len();
                            write_frame(&mut writer, &TerminalFrame {
                                kind: TerminalFrameKind::Output,
                                payload: bytes,
                            }).await?;
                            debug!(byte_count, "sent droid PTY output");
                        }
                        Some(TerminalEvent::OutputClosed) => {
                            output_closed = true;
                            if let Some(code) = pending_exit {
                                write_frame(&mut writer, &TerminalFrame {
                                    kind: TerminalFrameKind::Exit,
                                    payload: exit_payload(code).to_vec(),
                                }).await?;
                                break 'terminal Ok(());
                            }
                        }
                        Some(TerminalEvent::Exit(code)) => {
                            pending_exit = Some(code);
                            if output_closed {
                                write_frame(&mut writer, &TerminalFrame {
                                    kind: TerminalFrameKind::Exit,
                                    payload: exit_payload(code).to_vec(),
                                }).await?;
                                break 'terminal Ok(());
                            }
                        }
                        None => {
                            if let Some(code) = pending_exit {
                                write_frame(&mut writer, &TerminalFrame {
                                    kind: TerminalFrameKind::Exit,
                                    payload: exit_payload(code).to_vec(),
                                }).await?;
                            }
                            break 'terminal Ok(());
                        }
                    }
                }
            }
        }
    }
    .await;
    if result.is_err() {
        if let Err(error) = shutdown_session(Arc::clone(&session), "stream_error").await {
            warn!(%error, "failed to clean up Droid PTY after stream error");
        }
    }
    result
}

async fn negotiate_terminal<R, W>(reader: &mut R, writer: &mut W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = match read_frame(reader).await {
        Ok(frame) => frame,
        Err(error) => {
            let _ = write_error(writer, "unsupported_peer", "unsupported terminal peer").await;
            return Err(anyhow!(error));
        }
    };
    if hello.kind != TerminalFrameKind::Hello {
        write_error(writer, "expected_hello", "expected hello frame").await?;
        return Err(anyhow!("expected terminal hello frame"));
    }
    let client: HelloPayload = match decode_frame_payload(&hello.payload) {
        Ok(payload) => payload,
        Err(error) => {
            write_error(writer, "invalid_hello", "invalid hello payload").await?;
            return Err(anyhow!(error));
        }
    };
    if client.min_version > TERMINAL_PROTOCOL_VERSION
        || client.max_version < TERMINAL_PROTOCOL_VERSION
    {
        write_error(
            writer,
            "unsupported_version",
            "terminal protocol version is unsupported",
        )
        .await?;
        return Err(anyhow!("unsupported terminal protocol version"));
    }
    let server = HelloPayload {
        min_version: TERMINAL_PROTOCOL_VERSION,
        max_version: TERMINAL_PROTOCOL_VERSION,
        features: terminal_features(),
    };
    write_frame(
        writer,
        &TerminalFrame {
            kind: TerminalFrameKind::Hello,
            payload: frame_payload_json(&server)?,
        },
    )
    .await?;
    Ok(())
}

fn terminal_features() -> Vec<String> {
    [
        "hello", "start", "output", "input", "resize", "close", "exit", "error",
    ]
    .iter()
    .map(|feature| (*feature).to_owned())
    .collect()
}

async fn write_error<W>(writer: &mut W, code: &str, message: &str) -> Result<(), TerminalWireError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &TerminalFrame {
            kind: TerminalFrameKind::Error,
            payload: frame_payload_json(&ErrorPayload {
                code: code.to_string(),
                message: message.to_string(),
            })?,
        },
    )
    .await
}

enum TerminalEvent {
    Output(Vec<u8>),
    OutputClosed,
    Exit(i32),
}

struct DroidTerminalSession {
    id: String,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    process_group_leader: Option<i32>,
    closed: AtomicBool,
}

struct SpawnedDroidTerminal {
    session: Arc<DroidTerminalSession>,
    reader: Box<dyn Read + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl DroidTerminalSession {
    fn spawn(
        id: String,
        droid_bin: PathBuf,
        cwd: Option<String>,
        size: TerminalSize,
    ) -> anyhow::Result<SpawnedDroidTerminal> {
        validate_size(size)?;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening Droid PTY pair")?;

        let mut cmd = CommandBuilder::new(droid_bin.as_os_str());
        if let Some(cwd) = cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
            cmd.cwd(cwd);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_string()),
        );

        let reader = pair
            .master
            .try_clone_reader()
            .context("cloning Droid PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("taking Droid PTY writer")?;
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning interactive `{}`", droid_bin.display()))?;
        let killer = child.clone_killer();
        let process_group_leader = pair
            .master
            .process_group_leader()
            .map(|process_group| process_group as i32);
        Ok(SpawnedDroidTerminal {
            session: Arc::new(Self {
                id,
                writer: Mutex::new(writer),
                master: Mutex::new(pair.master),
                killer: Mutex::new(killer),
                process_group_leader,
                closed: AtomicBool::new(false),
            }),
            reader,
            child,
        })
    }

    fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().expect("droid PTY writer mutex poisoned");
        writer.write_all(data).context("writing Droid PTY input")?;
        writer.flush().context("flushing Droid PTY input")
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        validate_size(size)?;
        let master = self.master.lock().expect("droid PTY master mutex poisoned");
        master
            .resize(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing Droid PTY")
    }

    fn shutdown(&self, reason: &str) -> anyhow::Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            debug!(
                session = %self.id,
                reason,
                "Droid PTY shutdown already completed"
            );
            return Ok(());
        }
        debug!(session = %self.id, reason, "shutting down Droid PTY session");
        #[cfg(unix)]
        if let Some(pgid) = self.process_group_leader {
            if let Err(error) = terminate_process_group(pgid, reason) {
                warn!(
                    session = %self.id,
                    pgid,
                    reason,
                    %error,
                    "Droid PTY process-group shutdown failed; falling back to child killer"
                );
            }
        }
        let mut killer = self.killer.lock().expect("droid PTY killer mutex poisoned");
        if let Err(error) = killer.kill() {
            debug!(
                session = %self.id,
                reason,
                %error,
                "Droid PTY child killer returned after shutdown"
            );
        }
        Ok(())
    }
}

async fn shutdown_session(
    session: Arc<DroidTerminalSession>,
    reason: &'static str,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || session.shutdown(reason))
        .await
        .context("joining droid PTY shutdown task")?
}

#[cfg(unix)]
fn terminate_process_group(pgid: i32, reason: &str) -> anyhow::Result<()> {
    let pgid = pgid as libc::pid_t;
    if pgid <= 1 {
        return Ok(());
    }
    signal_process_group(pgid, libc::SIGTERM)
        .with_context(|| format!("sending SIGTERM to Droid PTY process group {pgid} ({reason})"))?;
    std::thread::sleep(SHUTDOWN_GRACE);
    if process_group_exists(pgid) {
        signal_process_group(pgid, libc::SIGKILL).with_context(|| {
            format!("sending SIGKILL to Droid PTY process group {pgid} after timeout ({reason})")
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn process_group_exists(pgid: libc::pid_t) -> bool {
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::killpg(pgid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn validate_size(size: TerminalSize) -> anyhow::Result<()> {
    if size.cols == 0 || size.rows == 0 {
        anyhow::bail!(
            "terminal size must be non-zero, got {}x{}",
            size.cols,
            size.rows
        );
    }
    Ok(())
}

fn spawn_output_thread(mut reader: Box<dyn Read + Send>, tx: mpsc::UnboundedSender<TerminalEvent>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(TerminalEvent::Output(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    debug!(%error, "Droid PTY reader ended");
                    break;
                }
            }
        }
        let _ = tx.send(TerminalEvent::OutputClosed);
    });
}

fn spawn_wait_thread(
    mut child: Box<dyn Child + Send + Sync>,
    tx: mpsc::UnboundedSender<TerminalEvent>,
) {
    std::thread::spawn(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(error) => {
                warn!(%error, "waiting for Droid PTY child failed");
                -1
            }
        };
        let _ = tx.send(TerminalEvent::Exit(code));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_jsonl_peer_without_echoing_secret_payload() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        server
            .write_all(br#"{"jsonrpc":"2.0","token":"terminal-secret-fixture"}"#)
            .await
            .unwrap();
        let error = read_frame(&mut client).await.unwrap_err().to_string();
        assert!(error.contains("unsupported terminal peer"));
        assert!(!error.contains("terminal-secret-fixture"));
    }

    #[tokio::test]
    async fn frame_round_trips_binary_payload() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let bytes = b"\x1b[?1049hpartial-\xf0\x9f\x90\x88\r\n".to_vec();
        let writer = tokio::spawn(async move {
            write_frame(
                &mut server,
                &TerminalFrame {
                    kind: TerminalFrameKind::Output,
                    payload: bytes.clone(),
                },
            )
            .await
            .unwrap();
            bytes
        });
        let frame = read_frame(&mut client).await.unwrap();
        let expected = writer.await.unwrap();
        assert_eq!(frame.kind, TerminalFrameKind::Output);
        assert_eq!(frame.payload, expected);
    }

    #[test]
    fn resize_payload_is_typed_and_binary() {
        let payload = resize_payload(TerminalSize {
            cols: 132,
            rows: 43,
        });
        assert_eq!(
            decode_resize_payload(&payload).unwrap(),
            TerminalSize {
                cols: 132,
                rows: 43
            }
        );
    }

    #[test]
    fn exit_payload_round_trips() {
        assert_eq!(decode_exit_payload(&exit_payload(130)).unwrap(), 130);
    }
}
