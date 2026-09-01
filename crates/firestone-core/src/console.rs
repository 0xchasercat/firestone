use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use nix::{
    errno::Errno,
    poll::{PollFd, PollFlags, poll},
    sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr},
    unistd::dup,
};
use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FirestoneError, Paths};

const CONSOLE_SOCKET_MODE: u32 = 0o600;
const CONSOLE_LOG_MODE: u32 = 0o600;
const CONSOLE_POLL_MS: u16 = 50;
const CONSOLE_BUFFER_BYTES: usize = 16 * 1024;
const MAX_PENDING_CLIENT_OUTPUT: usize = 1024 * 1024;
const CONSOLE_REPLY_MAX_BYTES: usize = 32;
const CONSOLE_OK: &[u8] = b"OK\n";
const CONSOLE_BUSY: &[u8] = b"BUSY\n";

/// The largest acknowledgement line the broker may send before it is refused.
///
/// The broker answers one short line and then switches to raw terminal bytes,
/// so a transport that reads more than this is not talking to a Firestone
/// console broker.
pub const CONSOLE_ACK_MAX_BYTES: usize = CONSOLE_REPLY_MAX_BYTES;

/// The broker's acknowledgement line, classified for any transport.
///
/// The blocking CLI path and the WebSocket transport read the line with
/// different I/O, but they must agree on what it means. This is that one
/// agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleAck {
    /// `OK`: this client owns the console and raw bytes follow.
    Ready,
    /// `BUSY`: another client already holds the single-client broker.
    Busy,
    /// Anything else, including a truncated or absent line.
    Invalid,
}

impl ConsoleAck {
    /// Classifies one acknowledgement line exactly as the broker writes it.
    #[must_use]
    pub fn classify(reply: &[u8]) -> Self {
        match reply {
            CONSOLE_OK => Self::Ready,
            CONSOLE_BUSY => Self::Busy,
            _ => Self::Invalid,
        }
    }
}

/// Paths needed to attach to one running machine console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolePlan {
    name: String,
    socket: PathBuf,
}

impl ConsolePlan {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The error a transport reports when the broker socket cannot be reached.
    ///
    /// The WebSocket transport connects asynchronously rather than through
    /// [`Self::connect`], so it needs the same `not_running` error without the
    /// blocking handshake.
    #[must_use]
    pub fn unavailable_error(&self, source: io::Error) -> FirestoneError {
        console_not_running(&self.name, &self.socket, source)
    }

    /// The error a transport reports for an unrecognized acknowledgement.
    #[must_use]
    pub fn invalid_ack_error(&self) -> FirestoneError {
        console_invalid_ack(&self.name)
    }

    /// Connects and completes the private broker acknowledgement.
    pub fn connect(&self, timeout: Duration) -> Result<UnixStream, FirestoneError> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|source| console_not_running(&self.name, &self.socket, source))?;
        stream.set_read_timeout(Some(timeout)).map_err(|source| {
            console_io_error(&self.name, "set the console handshake timeout", source)
        })?;
        stream.set_write_timeout(Some(timeout)).map_err(|source| {
            console_io_error(&self.name, "set the console handshake timeout", source)
        })?;
        let reply = read_reply(&mut stream, &self.name)?;
        match reply.as_slice() {
            CONSOLE_OK => {}
            CONSOLE_BUSY => {
                return Err(FirestoneError::new(
                    ErrorKind::Busy,
                    format!(
                        "machine `{}` console already has an attached client",
                        self.name
                    ),
                )
                .with_hint("detach the other console with Ctrl-] and retry"));
            }
            _ => {
                return Err(console_invalid_ack(&self.name));
            }
        }
        stream.set_read_timeout(None).map_err(|source| {
            console_io_error(&self.name, "clear the console handshake timeout", source)
        })?;
        stream.set_write_timeout(None).map_err(|source| {
            console_io_error(&self.name, "clear the console handshake timeout", source)
        })?;
        stream.set_nonblocking(true).map_err(|source| {
            console_io_error(
                &self.name,
                "make the console connection nonblocking",
                source,
            )
        })?;
        Ok(stream)
    }
}

/// Validates a Paths-owned mode-0600 console broker socket.
pub fn console_plan(paths: &Paths, name: &str) -> Result<ConsolePlan, FirestoneError> {
    paths.validate_machine_runtime_dir(name)?;
    let socket = paths.machine_console_socket(name)?;
    let metadata = fs::symlink_metadata(&socket)
        .map_err(|source| console_not_running(name, &socket, source))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.file_type().is_socket()
        || metadata.file_type().is_symlink()
        || metadata.uid() != paths.uid()
        || mode != CONSOLE_SOCKET_MODE
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "machine `{name}` console socket '{}' is insecure: expected a current-user mode-0600 Unix socket",
                socket.display()
            ),
        )
        .with_hint("stop and start the machine to recreate its private runtime sockets"));
    }
    Ok(ConsolePlan {
        name: name.to_owned(),
        socket,
    })
}

/// Why a console relay returned successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleResult {
    Detached,
    Disconnected,
}

/// Relays binary terminal bytes until Ctrl-] detaches or the broker closes.
pub fn relay_console<I, O>(
    name: &str,
    stream: &mut UnixStream,
    input: &mut I,
    output: &mut O,
    cancelled: &AtomicBool,
) -> Result<ConsoleResult, FirestoneError>
where
    I: Read + AsFd,
    O: Write,
{
    let mut input_buffer = [0_u8; CONSOLE_BUFFER_BYTES];
    let mut socket_buffer = [0_u8; CONSOLE_BUFFER_BYTES];
    let mut pending = Vec::new();
    let mut pending_offset = 0_usize;

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(FirestoneError::new(
                ErrorKind::Interrupted,
                format!("console attachment for machine `{name}` was interrupted"),
            ));
        }

        let (input_events, socket_events) = {
            let socket_interest = PollFlags::POLLIN
                | PollFlags::POLLHUP
                | PollFlags::POLLERR
                | if pending_offset < pending.len() {
                    PollFlags::POLLOUT
                } else {
                    PollFlags::empty()
                };
            let mut descriptors = [
                PollFd::new(input.as_fd(), PollFlags::POLLIN),
                PollFd::new(stream.as_fd(), socket_interest),
            ];
            if let Err(source) = poll(&mut descriptors, CONSOLE_POLL_MS) {
                if source == Errno::EINTR && cancelled.load(Ordering::Relaxed) {
                    return Err(FirestoneError::new(
                        ErrorKind::Interrupted,
                        format!("console attachment for machine {name} was interrupted"),
                    ));
                }
                return Err(console_io_error(
                    name,
                    "poll the terminal and console socket",
                    io::Error::from(source),
                ));
            }
            (
                descriptors[0].revents().unwrap_or_else(PollFlags::empty),
                descriptors[1].revents().unwrap_or_else(PollFlags::empty),
            )
        };

        if socket_events.contains(PollFlags::POLLOUT) && pending_offset < pending.len() {
            match stream.write(&pending[pending_offset..]) {
                Ok(0) => return Ok(ConsoleResult::Disconnected),
                Ok(written) => {
                    pending_offset += written;
                    if pending_offset == pending.len() {
                        pending.clear();
                        pending_offset = 0;
                    }
                }
                Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) if console_closed(&source) => return Ok(ConsoleResult::Disconnected),
                Err(source) => {
                    return Err(console_io_error(name, "write console input", source));
                }
            }
        }

        if input_events.contains(PollFlags::POLLIN) && pending_offset == pending.len() {
            let read = input
                .read(&mut input_buffer)
                .map_err(|source| console_io_error(name, "read terminal input", source))?;
            if read == 0 {
                return Ok(ConsoleResult::Detached);
            }
            if let Some(escape) = input_buffer[..read].iter().position(|byte| *byte == 0x1d) {
                if escape > 0 {
                    pending.extend_from_slice(&input_buffer[..escape]);
                    flush_pending(name, stream, &pending, &mut pending_offset)?;
                }
                return Ok(ConsoleResult::Detached);
            }
            pending.extend_from_slice(&input_buffer[..read]);
            flush_pending(name, stream, &pending, &mut pending_offset)?;
            if pending_offset == pending.len() {
                pending.clear();
                pending_offset = 0;
            }
        }

        if socket_events.contains(PollFlags::POLLIN) {
            match stream.read(&mut socket_buffer) {
                Ok(0) => return Ok(ConsoleResult::Disconnected),
                Ok(read) => {
                    output
                        .write_all(&socket_buffer[..read])
                        .and_then(|()| output.flush())
                        .map_err(|source| console_io_error(name, "write console output", source))?;
                }
                Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) if console_closed(&source) => return Ok(ConsoleResult::Disconnected),
                Err(source) => {
                    return Err(console_io_error(name, "read console output", source));
                }
            }
        }

        if socket_events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            return Ok(ConsoleResult::Disconnected);
        }
    }
}

fn flush_pending(
    name: &str,
    stream: &mut UnixStream,
    pending: &[u8],
    offset: &mut usize,
) -> Result<(), FirestoneError> {
    while *offset < pending.len() {
        match stream.write(&pending[*offset..]) {
            Ok(0) => return Ok(()),
            Ok(written) => *offset += written,
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(source) if console_closed(&source) => return Ok(()),
            Err(source) => return Err(console_io_error(name, "write console input", source)),
        }
    }
    Ok(())
}

/// Restores one terminal's exact prior termios state on explicit restore or drop.
pub struct RawTerminal {
    fd: OwnedFd,
    original: Option<Termios>,
}

fn terminal_setup_error(operation: &str, source: nix::errno::Errno) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, format!("cannot {operation}"))
        .with_hint("run the command from an interactive terminal that supports termios")
        .with_source(io::Error::from(source))
}

impl RawTerminal {
    pub fn enter<F: AsFd>(terminal: &F) -> Result<Self, FirestoneError> {
        let fd = dup(terminal.as_fd())
            .map_err(|source| terminal_setup_error("duplicate the terminal descriptor", source))?;
        let original =
            tcgetattr(&fd).map_err(|source| terminal_setup_error("read terminal mode", source))?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&fd, SetArg::TCSANOW, &raw)
            .map_err(|source| terminal_setup_error("switch terminal to raw mode", source))?;
        Ok(Self {
            fd,
            original: Some(original),
        })
    }

    pub fn restore(&mut self) -> Result<(), FirestoneError> {
        let Some(original) = self.original.take() else {
            return Ok(());
        };
        tcsetattr(&self.fd, SetArg::TCSANOW, &original).map_err(|source| {
            FirestoneError::new(ErrorKind::Generic, "cannot restore terminal mode")
                .with_hint("run `reset` to restore the terminal")
                .with_source(io::Error::from(source))
        })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Lifetime owner for a shim-side PTY broker thread.
pub struct ConsoleBroker {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), FirestoneError>>>,
    paths: Paths,
    name: String,
}

impl ConsoleBroker {
    /// Opens Cloud Hypervisor's published PTY peer and starts the broker.
    pub fn start(paths: &Paths, name: &str, pty_path: &Path) -> Result<Self, FirestoneError> {
        let pty = open_pty(paths, name, pty_path)?;
        Self::start_with_file(paths, name, pty)
    }

    fn start_with_file(paths: &Paths, name: &str, pty: File) -> Result<Self, FirestoneError> {
        paths.validate_machine_data_directory(name)?;
        paths.validate_machine_runtime_dir(name)?;
        let socket = paths.machine_console_socket(name)?;
        let listener = UnixListener::bind(&socket)
            .map_err(|source| console_io_error(name, "bind the console broker socket", source))?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(CONSOLE_SOCKET_MODE)).map_err(
            |source| console_io_error(name, "protect the console broker socket", source),
        )?;
        listener.set_nonblocking(true).map_err(|source| {
            console_io_error(name, "make the console broker socket nonblocking", source)
        })?;
        let log = open_pty_log(paths, name)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_name = name.to_owned();
        let worker = thread::Builder::new()
            .name(format!("firestone-console-{name}"))
            .spawn(move || broker_loop(&worker_name, listener, pty, log, &worker_cancelled))
            .map_err(|source| console_io_error(name, "start the console broker thread", source))?;
        Ok(Self {
            cancelled,
            worker: Some(worker),
            paths: paths.clone(),
            name: name.to_owned(),
        })
    }

    pub fn shutdown(mut self) -> Result<(), FirestoneError> {
        self.cancelled.store(true, Ordering::Release);
        let worker_result = match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| {
                FirestoneError::new(ErrorKind::Generic, "console broker thread panicked")
            })?,
            None => Ok(()),
        };
        let merge_result = merge_pty_log(&self.paths, &self.name);
        match (worker_result, merge_result) {
            (Err(error), _) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

impl Drop for ConsoleBroker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn broker_loop(
    name: &str,
    listener: UnixListener,
    mut pty: File,
    mut log: File,
    cancelled: &AtomicBool,
) -> Result<(), FirestoneError> {
    let mut active: Option<UnixStream> = None;
    let mut pty_buffer = [0_u8; CONSOLE_BUFFER_BYTES];
    let mut client_buffer = [0_u8; CONSOLE_BUFFER_BYTES];
    let mut guest_input = Vec::new();
    let mut guest_input_offset = 0_usize;
    let mut client_output = Vec::new();
    let mut client_output_offset = 0_usize;

    while !cancelled.load(Ordering::Acquire) {
        if active.is_none() {
            client_output.clear();
            client_output_offset = 0;
        }
        let (listener_events, pty_events, client_events) = {
            let pty_interest = PollFlags::POLLIN
                | PollFlags::POLLERR
                | PollFlags::POLLHUP
                | if guest_input_offset < guest_input.len() {
                    PollFlags::POLLOUT
                } else {
                    PollFlags::empty()
                };
            let mut descriptors = Vec::with_capacity(3);
            descriptors.push(PollFd::new(listener.as_fd(), PollFlags::POLLIN));
            descriptors.push(PollFd::new(pty.as_fd(), pty_interest));
            if let Some(client) = active.as_ref() {
                let client_interest = PollFlags::POLLIN
                    | PollFlags::POLLERR
                    | PollFlags::POLLHUP
                    | if client_output_offset < client_output.len() {
                        PollFlags::POLLOUT
                    } else {
                        PollFlags::empty()
                    };
                descriptors.push(PollFd::new(client.as_fd(), client_interest));
            }
            poll(&mut descriptors, CONSOLE_POLL_MS).map_err(|source| {
                console_io_error(name, "poll the console broker", io::Error::from(source))
            })?;
            (
                descriptors[0].revents().unwrap_or_else(PollFlags::empty),
                descriptors[1].revents().unwrap_or_else(PollFlags::empty),
                descriptors
                    .get(2)
                    .and_then(PollFd::revents)
                    .unwrap_or_else(PollFlags::empty),
            )
        };

        if listener_events.contains(PollFlags::POLLIN) {
            accept_clients(name, &listener, &mut active)?;
        }

        if pty_events.contains(PollFlags::POLLOUT) && guest_input_offset < guest_input.len() {
            match pty.write(&guest_input[guest_input_offset..]) {
                Ok(0) => {}
                Ok(written) => {
                    guest_input_offset += written;
                    if guest_input_offset == guest_input.len() {
                        guest_input.clear();
                        guest_input_offset = 0;
                    }
                }
                Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) => return Err(console_io_error(name, "write the console PTY", source)),
            }
        }

        if client_events.contains(PollFlags::POLLOUT) && client_output_offset < client_output.len()
        {
            if let Some(client) = active.as_mut() {
                match client.write(&client_output[client_output_offset..]) {
                    Ok(0) => active = None,
                    Ok(written) => {
                        client_output_offset += written;
                        if client_output_offset == client_output.len() {
                            client_output.clear();
                            client_output_offset = 0;
                        }
                    }
                    Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                    Err(source) if console_closed(&source) => active = None,
                    Err(source) => {
                        return Err(console_io_error(
                            name,
                            "relay PTY output to the console client",
                            source,
                        ));
                    }
                }
            }
        }

        if pty_events.contains(PollFlags::POLLIN) {
            match pty.read(&mut pty_buffer) {
                Ok(0) => {}
                Ok(read) => {
                    log.write_all(&pty_buffer[..read]).map_err(|source| {
                        console_io_error(name, "append PTY output to its staging log", source)
                    })?;
                    if active.is_some() {
                        if client_output_offset > 0 {
                            client_output.copy_within(client_output_offset.., 0);
                            client_output.truncate(client_output.len() - client_output_offset);
                            client_output_offset = 0;
                        }
                        if client_output.len().saturating_add(read) > MAX_PENDING_CLIENT_OUTPUT {
                            active = None;
                            client_output.clear();
                        } else {
                            client_output.extend_from_slice(&pty_buffer[..read]);
                        }
                    }
                }
                Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                Err(source) if source.raw_os_error() == Some(libc_eio()) => {}
                Err(source) => return Err(console_io_error(name, "read the console PTY", source)),
            }
        }

        if client_events.contains(PollFlags::POLLIN) && guest_input_offset == guest_input.len() {
            if let Some(client) = active.as_mut() {
                match client.read(&mut client_buffer) {
                    Ok(0) => active = None,
                    Ok(read) => guest_input.extend_from_slice(&client_buffer[..read]),
                    Err(source) if source.kind() == io::ErrorKind::WouldBlock => {}
                    Err(source) if console_closed(&source) => active = None,
                    Err(source) => {
                        return Err(console_io_error(
                            name,
                            "read input from the console client",
                            source,
                        ));
                    }
                }
            }
        }
        if client_events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            active = None;
            client_output.clear();
            client_output_offset = 0;
        }
    }
    Ok(())
}

fn accept_clients(
    name: &str,
    listener: &UnixListener,
    active: &mut Option<UnixStream>,
) -> Result<(), FirestoneError> {
    loop {
        let (mut client, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(source) => return Err(console_io_error(name, "accept a console client", source)),
        };
        client
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(|source| console_io_error(name, "bound a console acknowledgement", source))?;
        if active.is_some() {
            let _ = client.write_all(CONSOLE_BUSY);
            continue;
        }
        client
            .write_all(CONSOLE_OK)
            .map_err(|source| console_io_error(name, "acknowledge a console client", source))?;
        client.set_write_timeout(None).map_err(|source| {
            console_io_error(name, "clear the console acknowledgement timeout", source)
        })?;
        client.set_nonblocking(true).map_err(|source| {
            console_io_error(name, "make a console client nonblocking", source)
        })?;
        *active = Some(client);
    }
}

fn open_pty(paths: &Paths, name: &str, path: &Path) -> Result<File, FirestoneError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| console_io_error(name, "inspect the Cloud Hypervisor PTY", source))?;
    if !metadata.file_type().is_char_device()
        || metadata.file_type().is_symlink()
        || metadata.uid() != paths.uid()
    {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "machine `{name}` console PTY '{}' is not a current-user character device",
                path.display()
            ),
        )
        .with_hint("restart the machine to request a fresh Cloud Hypervisor PTY"));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(
            nix::libc::O_CLOEXEC
                | nix::libc::O_NOCTTY
                | nix::libc::O_NONBLOCK
                | nix::libc::O_NOFOLLOW,
        )
        .open(path)
        .map_err(|source| console_io_error(name, "open the Cloud Hypervisor PTY", source))
}

fn open_pty_log(paths: &Paths, name: &str) -> Result<File, FirestoneError> {
    let path = paths.machine_console_pty_log(name)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(CONSOLE_LOG_MODE)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|source| console_io_error(name, "create the PTY staging log", source))?;
    validate_runtime_log(paths, name, &path, &file)?;
    Ok(file)
}

fn merge_pty_log(paths: &Paths, name: &str) -> Result<(), FirestoneError> {
    paths.validate_machine_data_directory(name)?;
    paths.validate_machine_runtime_dir(name)?;
    let staging_path = paths.machine_console_pty_log(name)?;
    let mut staging = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&staging_path)
        .map_err(|source| console_io_error(name, "open the PTY staging log", source))?;
    validate_runtime_log(paths, name, &staging_path, &staging)?;

    let console_path = paths.machine_console_log(name)?;
    let mut console = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(CONSOLE_LOG_MODE)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&console_path)
        .map_err(|source| console_io_error(name, "open console.log after VMM exit", source))?;
    paths.validate_owned_data_file_handle(
        &console_path,
        "console log",
        CONSOLE_LOG_MODE,
        &console,
    )?;
    io::copy(&mut staging, &mut console)
        .map_err(|source| console_io_error(name, "merge PTY output into console.log", source))?;
    console
        .sync_all()
        .map_err(|source| console_io_error(name, "sync merged console.log", source))?;
    drop(staging);
    fs::remove_file(&staging_path)
        .map_err(|source| console_io_error(name, "remove the merged PTY staging log", source))
}

fn validate_runtime_log(
    paths: &Paths,
    name: &str,
    path: &Path,
    file: &File,
) -> Result<(), FirestoneError> {
    let metadata = file
        .metadata()
        .map_err(|source| console_io_error(name, "inspect the open PTY staging log", source))?;
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file() || metadata.uid() != paths.uid() || mode != CONSOLE_LOG_MODE {
        return Err(FirestoneError::new(
            ErrorKind::Dependency,
            format!(
                "machine {name} PTY staging log '{}' is insecure: expected a current-user mode-0600 regular file",
                path.display()
            ),
        )
        .with_hint("remove the insecure runtime log and retry the machine operation"));
    }
    Ok(())
}

fn read_reply(stream: &mut UnixStream, name: &str) -> Result<Vec<u8>, FirestoneError> {
    let mut reply = Vec::with_capacity(CONSOLE_REPLY_MAX_BYTES);
    let mut byte = [0_u8; 1];
    while reply.len() < CONSOLE_REPLY_MAX_BYTES {
        let read = stream.read(&mut byte).map_err(|source| {
            console_io_error(name, "read the console broker acknowledgement", source)
        })?;
        if read == 0 {
            break;
        }
        reply.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(reply);
        }
    }
    Ok(reply)
}

fn console_not_running(name: &str, path: &Path, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::NotRunning,
        format!(
            "machine `{name}` console is unavailable at '{}'",
            path.display()
        ),
    )
    .with_hint(format!(
        "start the machine with `firestone start {name}` and retry"
    ))
    .with_source(source)
}

fn console_invalid_ack(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Conflict,
        format!("machine `{name}` console broker returned an invalid acknowledgement"),
    )
    .with_hint("restart the machine to replace the stale console broker")
}

fn console_io_error(name: &str, phase: &str, source: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {phase} for machine `{name}`"),
    )
    .with_hint(
        "retry the console connection; if it fails again, inspect the machine shim and console logs",
    )
    .with_source(source)
}

fn console_closed(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

const fn libc_eio() -> i32 {
    nix::libc::EIO
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read as _, Write as _},
        os::{
            fd::AsFd as _,
            unix::fs::{MetadataExt as _, PermissionsExt as _},
        },
        sync::atomic::AtomicBool,
        thread,
        time::{Duration, Instant},
    };

    use nix::{
        poll::{PollFd, PollFlags, poll},
        pty::openpty,
        sys::termios::{LocalFlags, SetArg, cfmakeraw, tcgetattr, tcsetattr},
        unistd::pipe,
    };
    use tempfile::TempDir;

    use crate::{ErrorKind, PathInputs, Paths};

    use super::{
        CONSOLE_SOCKET_MODE, ConsoleBroker, ConsoleResult, RawTerminal, console_io_error,
        console_plan, relay_console,
    };

    #[test]
    fn console_io_failure_has_machine_context_and_hint() {
        let error = console_io_error(
            "demo",
            "relay console output",
            std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        );

        assert_eq!(error.kind(), ErrorKind::Generic);
        assert!(error.message().contains("demo"));
        assert!(error.message().contains("relay console output"));
        assert!(error.hint().is_some());
    }

    struct Fixture {
        _temp: TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = TempDir::new()?;
            let root = fs::canonicalize(temp.path())?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.clone(),
                home_dir: None,
                firestone_home: Some(root.join("home")),
                firestone_config_dir: None,
                firestone_data_dir: None,
                firestone_runtime_dir: None,
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: fs::metadata(&root)?.uid(),
            })?;
            paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
            paths.ensure_owned_data_directory(
                &paths.machines_dir(),
                "machines directory",
                false,
            )?;
            paths.ensure_owned_data_directory(
                &paths.machine_dir("demo")?,
                "machine directory",
                false,
            )?;
            paths.ensure_machine_runtime_dir("demo")?;
            Ok(Self { _temp: temp, paths })
        }
    }

    #[test]
    fn broker_attach_detach_busy_reattach_relays_and_logs_binary_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let opened = openpty(None, None)?;
        let mut termios = tcgetattr(&opened.slave)?;
        cfmakeraw(&mut termios);
        tcsetattr(&opened.slave, SetArg::TCSANOW, &termios)?;
        let mut guest = fs::File::from(opened.master);
        let broker =
            ConsoleBroker::start_with_file(&fixture.paths, "demo", fs::File::from(opened.slave))?;
        let socket = fixture.paths.machine_console_socket("demo")?;
        assert_eq!(
            fs::symlink_metadata(socket)?.mode() & 0o7777,
            CONSOLE_SOCKET_MODE
        );

        let plan = console_plan(&fixture.paths, "demo")?;
        let mut first = plan.connect(Duration::from_secs(1))?;
        let busy = plan
            .connect(Duration::from_secs(1))
            .err()
            .ok_or("second console attach unexpectedly succeeded")?;
        assert_eq!(busy.kind(), ErrorKind::Busy);

        let output = [0_u8, b'g', b'u', b'e', b's', b't', 0xff];
        guest.write_all(&output)?;
        {
            let mut descriptors = [PollFd::new(first.as_fd(), PollFlags::POLLIN)];
            assert!(
                poll(&mut descriptors, 1_000_u16)? > 0,
                "console output timed out"
            );
        }
        let mut received = [0_u8; 7];
        first.read_exact(&mut received)?;
        assert_eq!(received, output);
        drop(first);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut second = loop {
            match plan.connect(Duration::from_millis(200)) {
                Ok(stream) => break stream,
                Err(error) if error.kind() == ErrorKind::Busy && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error.into()),
            }
        };
        second.write_all(b"host-input")?;
        let mut descriptors = [PollFd::new(guest.as_fd(), PollFlags::POLLIN)];
        assert!(
            poll(&mut descriptors, 1_000_u16)? > 0,
            "guest input timed out"
        );
        let mut input = [0_u8; 10];
        guest.read_exact(&mut input)?;
        assert_eq!(&input, b"host-input");
        drop(second);
        thread::sleep(Duration::from_millis(100));
        broker.shutdown()?;
        assert_eq!(
            fs::read(fixture.paths.machine_console_log("demo")?)?,
            output
        );
        Ok(())
    }

    #[test]
    fn relay_ctrl_right_bracket_detaches_without_forwarding_suffix()
    -> Result<(), Box<dyn std::error::Error>> {
        let (input, input_writer) = pipe()?;
        let mut input_writer = fs::File::from(input_writer);
        input_writer.write_all(b"before\x1dafter")?;
        drop(input_writer);
        let mut input = fs::File::from(input);
        let (mut client, mut server) = std::os::unix::net::UnixStream::pair()?;
        client.set_nonblocking(true)?;
        let mut output = Vec::new();
        let result = relay_console(
            "demo",
            &mut client,
            &mut input,
            &mut output,
            &AtomicBool::new(false),
        )?;
        assert_eq!(result, ConsoleResult::Detached);
        server.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut received = [0_u8; 6];
        server.read_exact(&mut received)?;
        assert_eq!(&received, b"before");
        assert!(output.is_empty());
        Ok(())
    }

    #[test]
    fn raw_terminal_rejects_non_terminal_with_hint() -> Result<(), Box<dyn std::error::Error>> {
        let not_a_terminal = fs::File::open("/dev/null")?;
        let error = RawTerminal::enter(&not_a_terminal)
            .err()
            .ok_or("/dev/null should not support termios")?;

        assert_eq!(error.kind(), ErrorKind::Dependency);
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn raw_terminal_drop_restores_original_mode() -> Result<(), Box<dyn std::error::Error>> {
        let opened = openpty(None, None)?;
        let terminal = fs::File::from(opened.slave);
        let original = tcgetattr(&terminal)?;
        {
            let _guard = RawTerminal::enter(&terminal)?;
            let raw = tcgetattr(&terminal)?;
            assert!(!raw.local_flags.contains(LocalFlags::ICANON));
            assert!(!raw.local_flags.contains(LocalFlags::ECHO));
        }
        let restored = tcgetattr(&terminal)?;
        assert!(restored.local_flags.contains(LocalFlags::ICANON));
        assert!(restored.local_flags.contains(LocalFlags::ECHO));
        assert_eq!(restored.input_flags, original.input_flags);
        assert_eq!(restored.output_flags, original.output_flags);
        assert_eq!(restored.control_flags, original.control_flags);
        assert_eq!(restored.control_chars, original.control_chars);
        Ok(())
    }
}
