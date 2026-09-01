//! WebSocket console and shell transports.
//!
//! A browser cannot open a Unix socket and cannot speak the SSH wire
//! protocol, so the two interactive surfaces the CLI already has are
//! projected onto WebSockets that carry raw terminal bytes:
//!
//! - `/v1/machines/{name}/console/ws` bridges the machine's existing
//!   single-client console broker. The broker acknowledgement is completed
//!   *before* the upgrade, so a second viewer is answered with a normal REST
//!   `409` instead of a WebSocket that closes immediately.
//! - `/v1/machines/{name}/shell/ws` allocates a host pseudo-terminal and runs
//!   the same OpenSSH argv `firestone shell` execs, relaying the master.
//!
//! Framing is identical on both routes. Binary frames are raw terminal bytes
//! in both directions. Text frames are JSON control messages: `{"resize":
//! {"rows":R,"cols":C}}` applies `TIOCSWINSZ` on the shell master and is
//! accepted-and-ignored on the console, whose geometry belongs to the guest's
//! serial line. Anything else in a text frame is ignored rather than fatal, so
//! a newer client can add controls without breaking an older server.

use std::{
    io,
    os::fd::{AsFd as _, OwnedFd},
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{
        State,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code},
    },
    response::Response,
};
use firestone_core::{
    Action, CONSOLE_ACK_MAX_BYTES, ConsoleAck, ErrorKind, FirestoneError, MachineStatus,
    MachineView, ProcessSignal, PtyPair, SshCommandPlan, console_plan, set_window_size,
    shell_ssh_plan, signal_process_group,
};
use futures_util::{SinkExt as _, StreamExt as _, stream::SplitSink};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf},
    net::UnixStream,
};

use super::{ApiPath, ApiState, dispatch_payload, error_response};

/// The relay read buffer, matched to the console broker's own buffer.
const RELAY_BUFFER_BYTES: usize = 16 * 1024;

/// How long the console broker has to acknowledge before the request fails.
const CONSOLE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The close reason a console relay ends with: the broker only stops when the
/// machine does.
const CONSOLE_EOF_REASON: &str = "machine stopped";

/// The close reason a shell relay ends with: the usual end of a shell session
/// is the person typing `exit`, not the machine going away.
const SHELL_EOF_REASON: &str = "session ended";

/// The terminal type advertised to the guest shell.
///
/// The browser terminal renders 256-colour SGR, and OpenSSH forwards `TERM`
/// from this process rather than from the request.
const SHELL_TERM: &str = "xterm-256color";

/// `read(2)` on a pseudo-terminal master reports `EIO`, not zero, once the
/// last slave descriptor is closed. That is this transport's end of file.
const PTY_HANGUP_ERRNO: i32 = nix::errno::Errno::EIO as i32;

/// `GET /v1/machines/{name}/console/ws`.
pub(super) async fn console(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(error) = require_running(&state, &name).await {
        return error_response(error);
    }
    let plan = match console_plan(&state.paths, &name) {
        Ok(plan) => plan,
        Err(error) => return error_response(error),
    };
    let mut stream = match UnixStream::connect(plan.socket()).await {
        Ok(stream) => stream,
        Err(source) => return error_response(plan.unavailable_error(source)),
    };
    let acknowledgement = match tokio::time::timeout(CONSOLE_ACK_TIMEOUT, read_ack(&mut stream))
        .await
    {
        Ok(Ok(line)) => ConsoleAck::classify(&line),
        Ok(Err(source)) => {
            return error_response(
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot read the console broker acknowledgement for machine `{name}`"),
                )
                .with_hint("retry the connection; if it fails again, restart the machine")
                .with_source(source),
            );
        }
        Err(_) => {
            return error_response(
                FirestoneError::new(
                    ErrorKind::Timeout,
                    format!(
                        "machine `{name}` console broker did not acknowledge within {} seconds",
                        CONSOLE_ACK_TIMEOUT.as_secs()
                    ),
                )
                .with_hint("restart the machine to replace the stalled console broker"),
            );
        }
    };
    match acknowledgement {
        ConsoleAck::Ready => {}
        ConsoleAck::Busy => {
            return error_response(
                FirestoneError::new(
                    ErrorKind::Busy,
                    format!("machine `{name}` console already has an attached client"),
                )
                .with_hint(format!(
                    "detach the other console with Ctrl-] and retry; `firestone console {name}` shares the same single-client broker"
                )),
            );
        }
        ConsoleAck::Invalid => return error_response(plan.invalid_ack_error()),
    }

    upgrade.on_upgrade(move |socket| async move {
        bridge(socket, stream, None, CONSOLE_EOF_REASON).await;
    })
}

/// `GET /v1/machines/{name}/shell/ws`.
pub(super) async fn shell(
    State(state): State<ApiState>,
    ApiPath(name): ApiPath,
    upgrade: WebSocketUpgrade,
) -> Response {
    let view = match require_running(&state, &name).await {
        Ok(view) => view,
        Err(error) => return error_response(error),
    };
    let executable = match crate::current_firestone_executable() {
        Ok(executable) => executable,
        Err(error) => return error_response(error),
    };
    let plan = match shell_ssh_plan(
        &state.paths,
        &executable,
        &name,
        &view.spec.user,
        true,
        Vec::new(),
    ) {
        Ok(plan) => plan,
        Err(error) => return error_response(error),
    };

    upgrade.on_upgrade(move |socket| async move {
        run_shell_session(socket, plan).await;
    })
}

/// Reads the machine through the shared `Show` action and demands `running`.
///
/// Both terminal transports reach a live per-machine runtime socket, so both
/// must answer a stopped machine with the standard not-running error rather
/// than a filesystem diagnostic about a missing socket.
async fn require_running(state: &ApiState, name: &str) -> Result<MachineView, FirestoneError> {
    let payload = dispatch_payload(
        &state.dispatcher,
        Action::Show {
            name: name.to_owned(),
            vmconfig: false,
        },
        "show",
    )
    .await?;
    let view: MachineView = serde_json::from_value(payload).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("the show result did not match the shared contract: {source}"),
        )
        .with_hint("this is a Firestone defect; report it with the version string")
    })?;
    if view.state.status == MachineStatus::Running {
        Ok(view)
    } else {
        Err(not_running(name))
    }
}

fn not_running(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::NotRunning,
        format!("machine `{name}` is not running"),
    )
    .with_hint(format!("start it with `firestone start {name}` and retry"))
}

/// Reads the broker's acknowledgement line, bounded to one short line.
async fn read_ack(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(CONSOLE_ACK_MAX_BYTES);
    let mut byte = [0_u8; 1];
    while line.len() < CONSOLE_ACK_MAX_BYTES {
        if stream.read(&mut byte).await? == 0 {
            break;
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(line)
}

/// Allocates the pseudo-terminal, runs OpenSSH on it, and relays the master.
async fn run_shell_session(socket: WebSocket, plan: SshCommandPlan) {
    let pair = match PtyPair::open().and_then(|pair| {
        pair.set_master_nonblocking()?;
        Ok(pair)
    }) {
        Ok(pair) => pair,
        Err(error) => return close_with_error(socket, &error).await,
    };
    let child = plan
        .command()
        .env("TERM", SHELL_TERM)
        .spawn_terminal_process_group(pair.slave());
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return close_with_error(socket, &error).await,
    };
    // The parent's own slave copy must go now: while it is open the master
    // never reports the session ending.
    let (master, slave) = pair.into_parts();
    drop(slave);

    let control = match master.try_clone() {
        Ok(control) => control,
        Err(source) => {
            let error = FirestoneError::new(
                ErrorKind::Generic,
                "cannot duplicate the pseudo-terminal master for resize control",
            )
            .with_hint("check the process file-descriptor limit and retry")
            .with_source(source);
            let _ = child.signal_group(ProcessSignal::Kill);
            let _ = child.wait();
            return close_with_error(socket, &error).await;
        }
    };

    let resizer = Resizer {
        terminal: control,
        process_group: child.process_group(),
    };
    let stream = match PtyStream::new(master) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = child.signal_group(ProcessSignal::Kill);
            let _ = child.wait();
            return close_with_error(socket, &error).await;
        }
    };

    bridge(socket, stream, Some(resizer), SHELL_EOF_REASON).await;

    // One session per connection: when the WebSocket goes, so does OpenSSH
    // and everything it started.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = child.signal_group(ProcessSignal::Kill);
        let _ = child.wait();
    })
    .await;
}

/// Why a relay stopped, which becomes the close frame the client sees.
enum Ending {
    /// The byte stream ended: the machine stopped, or the shell exited.
    ///
    /// The reason names whichever of the two this route can mean, so a client
    /// is not told a running machine stopped because someone typed `exit`.
    Eof(&'static str),
    /// The byte stream failed.
    Failed(String),
    /// The client went away first; nothing is left to tell it.
    ClientGone,
}

/// Relays raw bytes between one WebSocket and one byte stream.
///
/// Backpressure is the await itself: the outbound task cannot read the next
/// chunk until the WebSocket has accepted the previous one, so a slow browser
/// slows the reader rather than growing an unbounded queue.
async fn bridge<S>(socket: WebSocket, stream: S, resize: Option<Resizer>, eof_reason: &'static str)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (sink, source) = socket.split();

    let mut outbound = tokio::spawn(async move {
        let mut reader = reader;
        let mut sink = sink;
        let ending = pump_out(&mut reader, &mut sink, eof_reason).await;
        close_sink(&mut sink, &ending).await;
    });
    let mut inbound = tokio::spawn(async move {
        let mut writer = writer;
        let mut source = source;
        while let Some(message) = source.next().await {
            let Ok(message) = message else {
                break;
            };
            match message {
                Message::Binary(bytes) => {
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Text(text) => apply_control(text.as_str(), resize.as_ref()),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
        let _ = writer.shutdown().await;
    });

    tokio::select! {
        _ = &mut outbound => inbound.abort(),
        _ = &mut inbound => outbound.abort(),
    }
}

async fn pump_out<R>(
    reader: &mut R,
    sink: &mut SplitSink<WebSocket, Message>,
    eof_reason: &'static str,
) -> Ending
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; RELAY_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ending::Eof(eof_reason),
            Ok(read) => {
                if sink
                    .send(Message::Binary(Bytes::copy_from_slice(&buffer[..read])))
                    .await
                    .is_err()
                {
                    return Ending::ClientGone;
                }
            }
            Err(source) => return Ending::Failed(source.to_string()),
        }
    }
}

/// The close frame an ending earns, or `None` when nobody is left to read it.
fn close_frame_for(ending: &Ending) -> Option<CloseFrame> {
    match ending {
        Ending::ClientGone => None,
        Ending::Eof(reason) => Some(CloseFrame {
            code: close_code::NORMAL,
            reason: Utf8Bytes::from(truncated_reason(reason)),
        }),
        Ending::Failed(detail) => Some(CloseFrame {
            code: close_code::ERROR,
            reason: Utf8Bytes::from(truncated_reason(detail)),
        }),
    }
}

async fn close_sink(sink: &mut SplitSink<WebSocket, Message>, ending: &Ending) {
    if let Some(frame) = close_frame_for(ending) {
        let _ = sink.send(Message::Close(Some(frame))).await;
    }
    let _ = sink.close().await;
}

async fn close_with_error(socket: WebSocket, error: &FirestoneError) {
    let mut socket = socket;
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: close_code::ERROR,
            reason: Utf8Bytes::from(truncated_reason(error.message())),
        })))
        .await;
}

/// A close reason must fit one control frame's 125-byte payload minus the code.
fn truncated_reason(detail: &str) -> String {
    const MAX_REASON_BYTES: usize = 120;
    let mut reason = String::with_capacity(MAX_REASON_BYTES);
    for character in detail.chars() {
        if reason.len() + character.len_utf8() > MAX_REASON_BYTES {
            break;
        }
        reason.push(character);
    }
    reason
}

/// What a resize control message acts on.
///
/// `TIOCSWINSZ` only raises `SIGWINCH` on the terminal's foreground process
/// group, and this child never claims the pseudo-terminal as its controlling
/// terminal — doing so needs `setsid` plus `TIOCSCTTY` between fork and exec,
/// which cannot be written where `unsafe_code` is forbidden. So the resize is
/// delivered in two explicit steps: set the size, then signal the group
/// directly. OpenSSH's `SIGWINCH` handler forwards the new size to the guest.
struct Resizer {
    terminal: OwnedFd,
    process_group: Option<u32>,
}

/// One JSON control message. Unknown members are ignored by construction.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Control {
    resize: Option<Resize>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Resize {
    rows: u16,
    cols: u16,
}

/// Applies one control message, ignoring everything it does not understand.
///
/// A malformed control frame never ends a terminal session: the byte stream is
/// the contract, and a resize is advisory.
fn apply_control(text: &str, resizer: Option<&Resizer>) {
    let Ok(control) = serde_json::from_str::<Control>(text) else {
        return;
    };
    let Some(resize) = control.resize else {
        return;
    };
    // The console broker owns a fixed guest serial geometry, so a resize on
    // that route is accepted and ignored rather than refused.
    let Some(resizer) = resizer else {
        return;
    };
    if set_window_size(resizer.terminal.as_fd(), resize.rows, resize.cols).is_err() {
        return;
    }
    if let Some(group) = resizer.process_group {
        let _ = signal_process_group(group, ProcessSignal::WindowChange);
    }
}

/// A pseudo-terminal master driven by the tokio reactor.
///
/// The master is a character device, so it has no `AsyncRead` of its own;
/// `AsyncFd` supplies readiness and the read itself is an ordinary
/// non-blocking `read(2)`.
struct PtyStream {
    inner: tokio::io::unix::AsyncFd<std::fs::File>,
}

impl PtyStream {
    fn new(master: OwnedFd) -> Result<Self, FirestoneError> {
        let file = std::fs::File::from(master);
        let inner = tokio::io::unix::AsyncFd::new(file).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                "cannot register the pseudo-terminal master with the async reactor",
            )
            .with_hint("retry the shell connection; if it fails again, report a Firestone bug")
            .with_source(source)
        })?;
        Ok(Self { inner })
    }
}

impl AsyncRead for PtyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use std::io::Read as _;

        let this = self.get_mut();
        loop {
            let mut guard = ready!(this.inner.poll_read_ready(context))?;
            let unfilled = buffer.initialize_unfilled();
            match guard.try_io(|inner| inner.get_ref().read(unfilled)) {
                Ok(Ok(read)) => {
                    buffer.advance(read);
                    return Poll::Ready(Ok(()));
                }
                // The last slave closed: report end of file, not an error.
                Ok(Err(source)) if source.raw_os_error() == Some(PTY_HANGUP_ERRNO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(source)) => return Poll::Ready(Err(source)),
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for PtyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        use std::io::Write as _;

        let this = self.get_mut();
        loop {
            let mut guard = ready!(this.inner.poll_write_ready(context))?;
            match guard.try_io(|inner| inner.get_ref().write(buffer)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::error::Error;

    use firestone_core::PtyPair;

    use super::{
        CONSOLE_EOF_REASON, Control, Ending, Resize, Resizer, SHELL_EOF_REASON, apply_control,
        close_code, close_frame_for, truncated_reason,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

    #[test]
    fn control_message_parsing_accepts_resize_and_ignores_everything_else() -> TestResult {
        let resize: Control = serde_json::from_str(r#"{"resize":{"rows":24,"cols":80}}"#)?;
        assert_eq!(
            resize,
            Control {
                resize: Some(Resize { rows: 24, cols: 80 }),
            }
        );

        // Unknown members are additive, not fatal.
        let extra: Control =
            serde_json::from_str(r#"{"resize":{"rows":50,"cols":132},"future":true}"#)?;
        assert_eq!(
            extra,
            Control {
                resize: Some(Resize {
                    rows: 50,
                    cols: 132
                }),
            }
        );

        // A control message with no resize is a no-op, not an error.
        let other: Control = serde_json::from_str(r#"{"signal":"INT"}"#)?;
        assert_eq!(other, Control { resize: None });

        // Out-of-range and malformed messages fail to parse and are ignored.
        assert!(serde_json::from_str::<Control>(r#"{"resize":{"rows":70000,"cols":80}}"#).is_err());
        assert!(serde_json::from_str::<Control>("not json").is_err());
        Ok(())
    }

    #[test]
    fn resize_control_applies_the_geometry_to_a_pseudo_terminal() -> TestResult {
        let pair = PtyPair::open()?;
        pair.resize(24, 80)?;
        let resizer = Resizer {
            terminal: pair.master().try_clone_to_owned()?,
            process_group: None,
        };

        apply_control(r#"{"resize":{"rows":50,"cols":132}}"#, Some(&resizer));
        let size = rustix::termios::tcgetwinsize(pair.slave())?;
        assert_eq!((size.ws_row, size.ws_col), (50, 132));

        // A zero dimension, a malformed frame and a frame with no resize all
        // leave the previous geometry in place.
        for ignored in [
            r#"{"resize":{"rows":0,"cols":132}}"#,
            r#"{"resize":{"rows":50}}"#,
            r#"{"ping":1}"#,
            "not json",
        ] {
            apply_control(ignored, Some(&resizer));
            let size = rustix::termios::tcgetwinsize(pair.slave())?;
            assert_eq!((size.ws_row, size.ws_col), (50, 132), "{ignored}");
        }
        Ok(())
    }

    #[test]
    fn resize_control_without_a_terminal_is_accepted_and_ignored() {
        // This is the console route: the guest owns its serial geometry.
        apply_control(r#"{"resize":{"rows":24,"cols":80}}"#, None);
    }

    #[test]
    fn close_reason_is_truncated_to_one_control_frame_on_a_character_boundary() {
        assert_eq!(truncated_reason("machine stopped"), "machine stopped");
        assert_eq!(truncated_reason(&"a".repeat(400)).len(), 120);

        // 'é' is two bytes: the cut must not split it.
        let multibyte = truncated_reason(&"é".repeat(400));
        assert!(multibyte.len() <= 120);
        assert_eq!(multibyte.chars().count(), 60);
    }

    #[test]
    fn end_of_stream_names_the_ending_the_route_can_mean() -> TestResult {
        let console = close_frame_for(&Ending::Eof(CONSOLE_EOF_REASON))
            .ok_or("an end of stream must close the socket")?;
        assert_eq!(console.code, close_code::NORMAL);
        assert_eq!(console.reason.as_str(), "machine stopped");

        let shell = close_frame_for(&Ending::Eof(SHELL_EOF_REASON))
            .ok_or("an end of stream must close the socket")?;
        assert_eq!(shell.code, close_code::NORMAL);
        assert_eq!(shell.reason.as_str(), "session ended");

        let failed = close_frame_for(&Ending::Failed("broken pipe".to_owned()))
            .ok_or("a failure must close the socket")?;
        assert_eq!(failed.code, close_code::ERROR);
        assert_eq!(failed.reason.as_str(), "broken pipe");

        // Nothing is left to tell: no frame is sent to a client that left.
        assert!(close_frame_for(&Ending::ClientGone).is_none());
        Ok(())
    }
}
