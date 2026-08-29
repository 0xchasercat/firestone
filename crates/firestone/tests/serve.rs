use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    pty::openpty,
    sys::signal::{Signal, kill},
    unistd::{Pid, getuid},
};
use wait_timeout::ChildExt as _;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let home = fs::canonicalize(directory.path())?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        let socket = home.join("run/serve.sock");
        Ok(Self {
            _directory: directory,
            home,
            socket,
        })
    }

    fn ensure_runtime(&self) -> TestResult<PathBuf> {
        let runtime = self.home.join("run");
        fs::create_dir(&runtime)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        Ok(runtime)
    }

    fn command(&self) -> Command {
        firestone(&self.home)
    }
}

struct Server {
    child: Option<Child>,
    socket: PathBuf,
}

impl Server {
    fn spawn(fixture: &Fixture) -> TestResult<Self> {
        let mut command = fixture.command();
        command.arg("serve");
        Self::spawn_command(fixture.socket.clone(), command)
    }

    fn spawn_command(socket: PathBuf, mut command: Command) -> TestResult<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut server = Self {
            child: Some(command.spawn()?),
            socket,
        };
        server.wait_until_ready()?;
        Ok(server)
    }

    fn wait_until_ready(&mut self) -> TestResult {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match fs::symlink_metadata(&self.socket) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    assert_eq!(metadata.mode() & 0o7777, 0o600);
                    assert_eq!(metadata.uid(), getuid().as_raw());
                    match UnixStream::connect(&self.socket) {
                        Ok(stream) => {
                            drop(stream);
                            return Ok(());
                        }
                        Err(source)
                            if matches!(
                                source.kind(),
                                std::io::ErrorKind::ConnectionRefused
                                    | std::io::ErrorKind::NotFound
                            ) => {}
                        Err(source) => return Err(source.into()),
                    }
                }
                Ok(_) => return Err("serve published a non-socket listener".into()),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(source.into()),
            }
            if let Some(status) = self.child_mut()?.try_wait()? {
                let stderr = read_child_stderr(self.child_mut()?)?;
                return Err(
                    format!("serve exited before readiness with {status}: {stderr}").into(),
                );
            }
            if Instant::now() >= deadline {
                return Err("serve did not publish its socket before the deadline".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn pid(&self) -> TestResult<Pid> {
        let id = self
            .child
            .as_ref()
            .ok_or("serve process already collected")?
            .id();
        Ok(Pid::from_raw(i32::try_from(id)?))
    }

    fn child_mut(&mut self) -> TestResult<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| "serve process already collected".into())
    }

    fn signal(mut self, signal: Signal) -> TestResult<Output> {
        kill(self.pid()?, signal)?;
        let child = self.child_mut()?;
        if child.wait_timeout(STOP_TIMEOUT)?.is_none() {
            child.kill()?;
            let _ = child.wait();
            return Err(format!("serve did not exit after {signal:?}").into());
        }
        let child = self.child.take().ok_or("serve process already collected")?;
        Ok(child.wait_with_output()?)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
            }
            if child
                .wait_timeout(Duration::from_millis(500))
                .ok()
                .flatten()
                .is_none()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn firestone(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.arg("--home").arg(home);
    command
}

fn run_bounded(mut command: Command, timeout: Duration) -> TestResult<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if child.wait_timeout(timeout)?.is_none() {
        child.kill()?;
        let _ = child.wait();
        return Err("command did not exit before its test deadline".into());
    }
    Ok(child.wait_with_output()?)
}

fn read_child_stderr(child: &mut Child) -> TestResult<String> {
    let mut stderr = String::new();
    if let Some(mut stream) = child.stderr.take() {
        stream.read_to_string(&mut stderr)?;
    }
    Ok(stderr)
}

fn http_request(socket: &Path, method: &str, path: &str, body: &[u8]) -> TestResult<Vec<u8>> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn response_status(response: &[u8]) -> TestResult<u16> {
    let line_end = response
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or("HTTP response has no status line")?;
    let line = std::str::from_utf8(&response[..line_end])?;
    Ok(line
        .split_whitespace()
        .nth(1)
        .ok_or("HTTP response has no status code")?
        .parse()?)
}

fn response_body(response: &[u8]) -> TestResult<&[u8]> {
    response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| &response[index + 4..])
        .ok_or_else(|| "HTTP response has no header terminator".into())
}

fn response_json(response: &[u8]) -> TestResult<serde_json::Value> {
    Ok(serde_json::from_slice(response_body(response)?)?)
}

fn assert_clean_text(bytes: &[u8]) {
    assert!(bytes.iter().all(|byte| {
        *byte == b'\n' || *byte == b'\r' || *byte == b'\t' || !byte.is_ascii_control()
    }));
    assert!(!bytes.contains(&0x1b));
}

fn wait_for_file(path: &Path, timeout: Duration) -> TestResult {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(format!("{} did not appear", path.display()).into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn wait_for_http(socket: &Path, timeout: Duration) -> TestResult<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = http_request(socket, "GET", "/v1/machines", b"") {
            if response_status(&response).ok() == Some(200) {
                return Ok(response);
            }
        }
        if Instant::now() >= deadline {
            return Err("serve did not answer HTTP before the deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn startup_failure(home: &Path) -> TestResult<Output> {
    let mut command = firestone(home);
    command.arg("serve");
    run_bounded(command, Duration::from_secs(2))
}

#[test]
fn serve_cli_help_and_incompatible_flags_fail_usage_before_binding() -> TestResult {
    let fixture = Fixture::new()?;
    let mut help = fixture.command();
    help.args(["serve", "--help"]);
    let help = run_bounded(help, COMMAND_TIMEOUT)?;
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let help_text = String::from_utf8(help.stdout)?;
    assert!(help_text.contains("Usage: firestone serve [OPTIONS]"));
    assert!(help_text.contains("--listen <unix:PATH>"));
    assert!(!help_text.contains("--token"));
    assert!(!help_text.contains("tcp:"));

    let mut tcp = fixture.command();
    tcp.args(["serve", "--listen", "tcp:127.0.0.1:8080"]);
    let tcp = run_bounded(tcp, COMMAND_TIMEOUT)?;
    assert_eq!(tcp.status.code(), Some(2));
    assert!(tcp.stdout.is_empty());
    assert_clean_text(&tcp.stderr);
    assert!(!fixture.home.join("run").exists());

    let mut yes = fixture.command();
    yes.args(["--json", "--yes", "serve"]);
    let yes = run_bounded(yes, COMMAND_TIMEOUT)?;
    assert_eq!(yes.status.code(), Some(2));
    assert!(yes.stderr.is_empty());
    assert_clean_text(&yes.stdout);
    let error: serde_json::Value = serde_json::from_slice(&yes.stdout)?;
    assert_eq!(error["error"]["kind"], "usage");
    assert_eq!(
        error["error"]["message"],
        "--yes is not valid with firestone serve"
    );
    assert!(!fixture.home.join("run").exists());
    let custom_socket = fixture.home.join("run/custom.sock");
    let mut custom = fixture.command();
    custom.args(["serve", "--listen", "unix:custom.sock"]);
    let custom = Server::spawn_command(custom_socket.clone(), custom)?;
    assert!(!fixture.socket.exists());
    assert_eq!(
        response_status(&http_request(&custom_socket, "GET", "/v1/machines", b"")?)?,
        200
    );
    assert!(custom.signal(Signal::SIGTERM)?.status.success());
    assert!(!custom_socket.exists());
    Ok(())
}

#[test]
fn serve_permissive_inherited_umask_still_publishes_private_nodes() -> TestResult {
    let fixture = Fixture::new()?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(r#"umask 000; exec "$1" --home "$2" serve"#)
        .arg("firestone-umask")
        .arg(env!("CARGO_BIN_EXE_firestone"))
        .arg(&fixture.home);
    let server = Server::spawn_command(fixture.socket.clone(), command)?;
    let runtime = fs::symlink_metadata(fixture.home.join("run"))?;
    let socket = fs::symlink_metadata(&fixture.socket)?;
    let lock = fs::symlink_metadata(fixture.home.join("run/.serve.lock"))?;
    assert_eq!(runtime.mode() & 0o7777, 0o700);
    assert_eq!(socket.mode() & 0o7777, 0o600);
    assert_eq!(lock.mode() & 0o7777, 0o600);
    assert!(server.signal(Signal::SIGTERM)?.status.success());
    Ok(())
}

#[test]
fn serve_real_dispatcher_http_smoke_concurrency_and_cli_lock_conflict() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;

    let version = http_request(&server.socket, "GET", "/v1/version", b"")?;
    assert_eq!(response_status(&version)?, 200);
    let version = response_json(&version)?;
    assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        version["paths"]["runtime"],
        fixture.home.join("run").to_string_lossy().as_ref()
    );

    let empty = http_request(&server.socket, "GET", "/v1/machines", b"")?;
    assert_eq!(response_status(&empty)?, 200);
    assert_eq!(response_body(&empty)?, b"[]");

    let create_body = br#"{"name":"demo","spec":{"image":"ubuntu:24.04"}}"#;
    let created = http_request(&server.socket, "POST", "/v1/machines", create_body)?;
    assert_eq!(response_status(&created)?, 201);
    let created_json = response_json(&created)?;
    assert_eq!(created_json["name"], "demo");
    assert_eq!(created_json["spec"]["image"], "ubuntu:24.04");

    let listed = http_request(&server.socket, "GET", "/v1/machines", b"")?;
    assert_eq!(response_status(&listed)?, 200);
    let listed_json = response_json(&listed)?;
    assert_eq!(listed_json.as_array().map(Vec::len), Some(1));
    assert_eq!(listed_json[0]["name"], "demo");

    let shown = http_request(&server.socket, "GET", "/v1/machines/demo", b"")?;
    assert_eq!(response_status(&shown)?, 200);
    let shown_json = response_json(&shown)?;
    assert_eq!(shown_json["spec"], created_json["spec"]);
    assert_eq!(shown_json["state"], created_json["state"]);
    assert!(shown_json.get("name").is_none());
    assert!(shown_json["supervision"].is_null());

    let mut clients = Vec::new();
    for _ in 0..16 {
        let socket = server.socket.clone();
        clients.push(thread::spawn(move || {
            http_request(&socket, "GET", "/v1/machines", b"")
        }));
    }
    for client in clients {
        let response = client.join().map_err(|_| "concurrent client panicked")??;
        assert_eq!(response_status(&response)?, 200);
        assert_eq!(response_json(&response)?[0]["name"], "demo");
    }

    let editor = fixture.home.join("blocking-editor.sh");
    let editor_ready = fixture.home.join("editor.ready");
    fs::write(
        &editor,
        b"#!/bin/sh\n: > \"$FIRESTONE_TEST_EDITOR_READY\"\nsleep 12\n",
    )?;
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700))?;
    let mut edit = fixture.command();
    edit.arg("edit")
        .arg("demo")
        .env("VISUAL", format!("sh {}", editor.display()))
        .env("FIRESTONE_TEST_EDITOR_READY", &editor_ready)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut edit = edit.spawn()?;
    wait_for_file(&editor_ready, Duration::from_secs(3))?;

    let started = Instant::now();
    let blocked = http_request(
        &server.socket,
        "PATCH",
        "/v1/machines/demo",
        br#"{"cpus":3}"#,
    )?;
    assert!(started.elapsed() >= Duration::from_secs(9));
    assert_eq!(response_status(&blocked)?, 409);
    assert_eq!(response_json(&blocked)?["error"]["kind"], "busy");
    if edit.wait_timeout(Duration::from_secs(5))?.is_none() {
        edit.kill()?;
        let _ = edit.wait();
        return Err("blocking editor did not finish".into());
    }
    assert!(edit.wait()?.success());

    let output = server.signal(Signal::SIGTERM)?;
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!fixture.socket.exists());
    Ok(())
}

#[test]
fn serve_conflicts_stale_takeover_and_simultaneous_start_leave_one_owner() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;

    let mut second = fixture.command();
    second.arg("serve");
    let second = run_bounded(second, COMMAND_TIMEOUT)?;
    assert_eq!(second.status.code(), Some(4));
    assert!(second.stdout.is_empty());
    assert_clean_text(&second.stderr);
    let stderr = String::from_utf8(second.stderr)?;
    assert!(stderr.contains("another firestone serve process owns the runtime lock"));

    let mut json = fixture.command();
    json.args(["--json", "serve"]);
    let json = run_bounded(json, COMMAND_TIMEOUT)?;
    assert_eq!(json.status.code(), Some(4));
    assert!(json.stderr.is_empty());
    assert_clean_text(&json.stdout);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&json.stdout)?["error"]["kind"],
        "conflict"
    );

    let stopped = server.signal(Signal::SIGINT)?;
    assert!(stopped.status.success());
    assert!(!fixture.socket.exists());
    let lock = fixture.home.join("run/.serve.lock");
    let lock_metadata = fs::symlink_metadata(&lock)?;
    assert!(lock_metadata.is_file());
    assert_eq!(lock_metadata.mode() & 0o7777, 0o600);

    let stale = UnixListener::bind(&fixture.socket)?;
    fs::set_permissions(&fixture.socket, fs::Permissions::from_mode(0o600))?;
    drop(stale);
    let replacement = Server::spawn(&fixture)?;
    let live = wait_for_http(&fixture.socket, START_TIMEOUT)?;
    assert_eq!(response_status(&live)?, 200);
    assert!(replacement.signal(Signal::SIGTERM)?.status.success());

    let simultaneous = Fixture::new()?;
    let mut first_command = simultaneous.command();
    first_command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second_command = simultaneous.command();
    second_command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut first = first_command.spawn()?;
    let mut second = second_command.spawn()?;
    let deadline = Instant::now() + START_TIMEOUT;
    let loser_is_first = loop {
        if first.try_wait()?.is_some() {
            break true;
        }
        if second.try_wait()?.is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            let _ = first.kill();
            let _ = second.kill();
            return Err("simultaneous serve attempts did not settle".into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (loser, owner) = if loser_is_first {
        (&mut first, &mut second)
    } else {
        (&mut second, &mut first)
    };
    assert_eq!(loser.wait()?.code(), Some(4));
    assert!(owner.try_wait()?.is_none());
    let response = wait_for_http(&simultaneous.socket, START_TIMEOUT)?;
    assert_eq!(response_status(&response)?, 200);
    kill(Pid::from_raw(i32::try_from(owner.id())?), Signal::SIGTERM)?;
    assert!(owner.wait_timeout(STOP_TIMEOUT)?.is_some());
    assert!(!simultaneous.socket.exists());
    Ok(())
}

#[test]
fn serve_hostile_runtime_nodes_and_ancestry_are_refused_untouched() -> TestResult {
    let regular = Fixture::new()?;
    regular.ensure_runtime()?;
    fs::write(&regular.socket, b"hostile")?;
    fs::set_permissions(&regular.socket, fs::Permissions::from_mode(0o600))?;
    let output = startup_failure(&regular.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(fs::read(&regular.socket)?, b"hostile");

    let linked = Fixture::new()?;
    let runtime = linked.ensure_runtime()?;
    let target = linked.home.join("target");
    fs::write(&target, b"target")?;
    symlink(&target, &linked.socket)?;
    let output = startup_failure(&linked.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert!(
        fs::symlink_metadata(&linked.socket)?
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&target)?, b"target");
    assert!(runtime.is_dir());

    let wrong_mode = Fixture::new()?;
    wrong_mode.ensure_runtime()?;
    let stale = UnixListener::bind(&wrong_mode.socket)?;
    fs::set_permissions(&wrong_mode.socket, fs::Permissions::from_mode(0o666))?;
    drop(stale);
    let output = startup_failure(&wrong_mode.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(
        fs::symlink_metadata(&wrong_mode.socket)?.mode() & 0o7777,
        0o666
    );

    let active = Fixture::new()?;
    active.ensure_runtime()?;
    let listener = UnixListener::bind(&active.socket)?;
    fs::set_permissions(&active.socket, fs::Permissions::from_mode(0o600))?;
    let output = startup_failure(&active.home)?;
    assert_eq!(output.status.code(), Some(4));
    assert!(
        fs::symlink_metadata(&active.socket)?
            .file_type()
            .is_socket()
    );
    drop(listener);

    let lock_link = Fixture::new()?;
    let runtime = lock_link.ensure_runtime()?;
    let target = lock_link.home.join("lock-target");
    fs::write(&target, b"lock")?;
    symlink(&target, runtime.join(".serve.lock"))?;
    let output = startup_failure(&lock_link.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert!(
        fs::symlink_metadata(runtime.join(".serve.lock"))?
            .file_type()
            .is_symlink()
    );

    let lock_mode = Fixture::new()?;
    let runtime = lock_mode.ensure_runtime()?;
    fs::write(runtime.join(".serve.lock"), b"")?;
    fs::set_permissions(
        runtime.join(".serve.lock"),
        fs::Permissions::from_mode(0o644),
    )?;
    let output = startup_failure(&lock_mode.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(
        fs::symlink_metadata(runtime.join(".serve.lock"))?.mode() & 0o7777,
        0o644
    );

    let runtime_link = Fixture::new()?;
    let target = runtime_link.home.join("target-runtime");
    fs::create_dir(&target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
    symlink(&target, runtime_link.home.join("run"))?;
    let output = startup_failure(&runtime_link.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert!(!target.join("serve.sock").exists());

    let runtime_mode = Fixture::new()?;
    let runtime = runtime_mode.ensure_runtime()?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755))?;
    let output = startup_failure(&runtime_mode.home)?;
    assert_eq!(output.status.code(), Some(5));
    assert!(!runtime.join("serve.sock").exists());

    let ancestry = Fixture::new()?;
    let unsafe_parent = ancestry.home.join("unsafe");
    fs::create_dir(&unsafe_parent)?;
    fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777))?;
    let nested_home = unsafe_parent.join("firestone");
    let output = startup_failure(&nested_home)?;
    assert_eq!(output.status.code(), Some(5));
    assert!(!nested_home.exists());
    Ok(())
}

#[test]
fn serve_signals_abrupt_restart_and_socket_identity_cleanup_are_safe() -> TestResult {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let fixture = Fixture::new()?;
        let server = Server::spawn(&fixture)?;
        let output = server.signal(signal)?;
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(!fixture.socket.exists());
    }

    let abrupt = Fixture::new()?;
    let server = Server::spawn(&abrupt)?;
    let before = fs::symlink_metadata(&abrupt.socket)?;
    let killed = server.signal(Signal::SIGKILL)?;
    assert!(!killed.status.success());
    let stale = fs::symlink_metadata(&abrupt.socket)?;
    assert_eq!((stale.dev(), stale.ino()), (before.dev(), before.ino()));
    let restarted = Server::spawn(&abrupt)?;
    assert_eq!(
        response_status(&http_request(&abrupt.socket, "GET", "/v1/version", b"")?)?,
        200
    );
    assert!(restarted.signal(Signal::SIGTERM)?.status.success());
    assert!(!abrupt.socket.exists());

    let identity = Fixture::new()?;
    let server = Server::spawn(&identity)?;
    fs::remove_file(&identity.socket)?;
    let replacement = UnixListener::bind(&identity.socket)?;
    fs::set_permissions(&identity.socket, fs::Permissions::from_mode(0o600))?;
    let replacement_metadata = fs::symlink_metadata(&identity.socket)?;
    let output = server.signal(Signal::SIGINT)?;
    assert!(output.status.success());
    let after = fs::symlink_metadata(&identity.socket)?;
    assert_eq!(
        (after.dev(), after.ino()),
        (replacement_metadata.dev(), replacement_metadata.ino())
    );
    drop(replacement);
    fs::remove_file(&identity.socket)?;
    Ok(())
}

#[test]
fn serve_conflict_tty_stderr_has_no_progress_or_escape_bytes() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;
    let opened = openpty(None, None)?;
    let mut master = File::from(opened.master);
    let terminal = File::from(opened.slave);
    let mut command = fixture.command();
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(terminal));
    let mut child = command.spawn()?;
    drop(command);
    let reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        match master.read_to_end(&mut bytes) {
            Ok(_) => Ok(bytes),
            Err(source) if source.raw_os_error() == Some(nix::libc::EIO) => Ok(bytes),
            Err(source) => Err(source),
        }
    });
    let status = child
        .wait_timeout(COMMAND_TIMEOUT)?
        .ok_or("TTY conflict command did not exit")?;
    assert_eq!(status.code(), Some(4));
    let bytes = reader.join().map_err(|_| "TTY reader panicked")??;
    assert_clean_text(&bytes);
    assert!(
        String::from_utf8_lossy(&bytes)
            .contains("another firestone serve process owns the runtime lock"),
        "TTY stderr was {:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(server.signal(Signal::SIGTERM)?.status.success());
    Ok(())
}
