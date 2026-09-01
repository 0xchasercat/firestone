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
    assert!(help_text.contains("--listen <unix:PATH|tcp:HOST:PORT>"));
    assert!(help_text.contains("--token <FILE>"));

    // SPEC 16.1 reserves exactly `--listen tcp:HOST:PORT --token FILE`, so a
    // TCP listener without a token file is a usage error before any bind.
    let mut tcp = fixture.command();
    tcp.args(["serve", "--listen", "tcp:127.0.0.1:8080"]);
    let tcp = run_bounded(tcp, COMMAND_TIMEOUT)?;
    assert_eq!(tcp.status.code(), Some(2));
    assert!(tcp.stdout.is_empty());
    assert_clean_text(&tcp.stderr);
    assert!(String::from_utf8_lossy(&tcp.stderr).contains("--token"));
    assert!(!fixture.home.join("run").exists());

    // A Unix socket is authenticated by its mode 0600, so a token there is a
    // usage error too, and neither form may name a routable address.
    let mut unix_token = fixture.command();
    unix_token.args(["serve", "--token", "/dev/null"]);
    let unix_token = run_bounded(unix_token, COMMAND_TIMEOUT)?;
    assert_eq!(unix_token.status.code(), Some(2));
    assert!(!fixture.home.join("run").exists());

    for address in ["tcp:0.0.0.0:8080", "tcp:[::]:8080", "tcp:192.168.1.10:8080"] {
        let mut routable = fixture.command();
        routable.args(["serve", "--listen", address, "--token", "/dev/null"]);
        let routable = run_bounded(routable, COMMAND_TIMEOUT)?;
        assert_eq!(routable.status.code(), Some(2), "{address}");
        assert!(routable.stdout.is_empty(), "{address}");
        assert_clean_text(&routable.stderr);
        assert!(!fixture.home.join("run").exists(), "{address}");
    }

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

/// A `firestone ui` process plus everything its printed URL announced.
struct UiServer {
    child: Option<Child>,
    lines: Vec<String>,
    port: u16,
    token: String,
}

impl UiServer {
    fn spawn(fixture: &Fixture, globals: &[&str], lines: usize) -> TestResult<Self> {
        let mut command = fixture.command();
        command.args(globals).arg("ui").arg("--print-url");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or("ui stdout was not piped")?;
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut collected = Vec::new();
            for _ in 0..lines {
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                    break;
                }
                collected.push(line);
            }
            let _ = sender.send(collected);
        });
        let collected = match receiver.recv_timeout(START_TIMEOUT) {
            Ok(collected) if collected.len() == lines => collected,
            other => {
                let _ = child.kill();
                let mut stderr = String::new();
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_string(&mut stderr);
                }
                let _ = child.wait();
                return Err(format!("ui did not announce its URL: {other:?}: {stderr}").into());
            }
        };
        let mut server = Self {
            child: Some(child),
            lines: collected,
            port: 0,
            token: String::new(),
        };
        let url = server.announced_url()?;
        let (address, token) = url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|rest| rest.split_once("/?token="))
            .ok_or("ui announced an unexpected URL shape")?;
        server.port = address.parse()?;
        server.token = token.to_owned();
        Ok(server)
    }

    fn announced_url(&self) -> TestResult<String> {
        let first = self.lines.first().ok_or("ui printed no URL line")?;
        if let Some(url) = first.trim_end().strip_prefix("Firestone UI   ") {
            return Ok(url.to_owned());
        }
        let record: serde_json::Value = serde_json::from_str(first.trim_end())?;
        Ok(record["url"]
            .as_str()
            .ok_or("ui JSON record has no url field")?
            .to_owned())
    }

    fn stop(mut self) -> TestResult<Output> {
        let child = self.child.as_mut().ok_or("ui process already collected")?;
        let pid = Pid::from_raw(i32::try_from(child.id())?);
        kill(pid, Signal::SIGTERM)?;
        if child.wait_timeout(STOP_TIMEOUT)?.is_none() {
            child.kill()?;
            let _ = child.wait();
            return Err("ui did not exit after SIGTERM".into());
        }
        let child = self.child.take().ok_or("ui process already collected")?;
        Ok(child.wait_with_output()?)
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn tcp_raw(port: u16, request: &[u8]) -> TestResult<Vec<u8>> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(request)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(response)
}

fn tcp_get(port: u16, path: &str, headers: &[(&str, &str)]) -> TestResult<Vec<u8>> {
    tcp_request(port, "GET", path, headers, b"")
}

fn tcp_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> TestResult<Vec<u8>> {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    let mut host_supplied = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            host_supplied = true;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !host_supplied {
        request.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    request.push_str(&format!(
        "Connection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ));
    let mut bytes = request.into_bytes();
    bytes.extend_from_slice(body);
    tcp_raw(port, &bytes)
}

fn response_header(response: &[u8], name: &str) -> TestResult<Option<String>> {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("HTTP response has no header terminator")?;
    let text = std::str::from_utf8(&response[..end])?;
    for line in text.split("\r\n").skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case(name) {
                return Ok(Some(value.trim().to_owned()));
            }
        }
    }
    Ok(None)
}

const EXPECTED_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

#[test]
fn ui_binds_an_ephemeral_loopback_port_and_announces_a_reachable_url() -> TestResult {
    let fixture = Fixture::new()?;
    let server = UiServer::spawn(&fixture, &[], 2)?;
    assert_ne!(server.port, 0, "the kernel must choose a real port");
    assert_eq!(server.token.len(), 64);
    assert!(server.token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(server.token.bytes().all(|byte| !byte.is_ascii_uppercase()));
    assert_eq!(
        server.lines[0],
        format!(
            "Firestone UI   http://127.0.0.1:{}/?token={}\n",
            server.port, server.token
        )
    );
    assert_eq!(server.lines[1], "Press Ctrl-C to stop.\n");

    let authorization = format!("Bearer {}", server.token);
    let version = tcp_get(
        server.port,
        "/v1/version",
        &[("Authorization", authorization.as_str())],
    )?;
    assert_eq!(response_status(&version)?, 200);
    assert_eq!(
        response_json(&version)?["version"],
        env!("CARGO_PKG_VERSION")
    );

    // The REST 404 contract is unchanged by the merged UI router.
    let missing = tcp_get(
        server.port,
        "/v1/nope",
        &[("Authorization", authorization.as_str())],
    )?;
    assert_eq!(response_status(&missing)?, 404);
    assert_eq!(
        response_header(&missing, "content-type")?.as_deref(),
        Some("application/json")
    );
    assert_eq!(
        response_json(&missing)?,
        serde_json::json!({
            "error": {
                "kind": "not_found",
                "message": "no REST route matches this request",
                "hint": "check the HTTP method and the /v1 route path"
            }
        })
    );

    // Security headers are on the JSON surface and on the UI surface alike.
    for response in [
        &version,
        &tcp_get(
            server.port,
            "/",
            &[("Authorization", authorization.as_str())],
        )?,
    ] {
        assert_eq!(
            response_header(response, "content-security-policy")?.as_deref(),
            Some(EXPECTED_CSP)
        );
        assert_eq!(
            response_header(response, "referrer-policy")?.as_deref(),
            Some("no-referrer")
        );
        assert_eq!(
            response_header(response, "x-content-type-options")?.as_deref(),
            Some("nosniff")
        );
        assert_eq!(
            response_header(response, "cross-origin-opener-policy")?.as_deref(),
            Some("same-origin")
        );
        assert_eq!(
            response_header(response, "cross-origin-resource-policy")?.as_deref(),
            Some("same-origin")
        );
    }

    let output = server.stop()?;
    assert!(output.status.success());
    Ok(())
}

#[test]
fn ui_loopback_transport_refuses_every_unauthenticated_or_rebound_request() -> TestResult {
    let fixture = Fixture::new()?;
    let server = UiServer::spawn(&fixture, &[], 2)?;
    let port = server.port;
    let token = server.token.clone();

    let absent = tcp_get(port, "/v1/version", &[])?;
    assert_eq!(response_status(&absent)?, 401);
    assert_eq!(response_json(&absent)?["error"]["kind"], "usage");
    assert_eq!(response_header(&absent, "www-authenticate")?, None);

    let mut flipped = token.clone();
    let last = flipped.pop().ok_or("token was empty")?;
    flipped.push(if last == '0' { '1' } else { '0' });
    for wrong in [flipped.as_str(), &"0".repeat(64), "short"] {
        let response = tcp_get(
            port,
            "/v1/version",
            &[("Authorization", &format!("Bearer {wrong}"))],
        )?;
        assert_eq!(response_status(&response)?, 401, "{wrong}");
        assert!(
            !String::from_utf8_lossy(&response).contains(&token),
            "an error response must never echo the real token"
        );
    }

    // DNS rebinding: the attacker's name resolves to 127.0.0.1 but the Host
    // header still names the attacker.
    for host in [
        format!("evil.example.com:{port}"),
        format!("firestone.attacker.test:{port}"),
    ] {
        let response = tcp_get(
            port,
            "/v1/version",
            &[
                ("Host", host.as_str()),
                ("Authorization", &format!("Bearer {token}")),
            ],
        )?;
        assert_eq!(response_status(&response)?, 403, "{host}");
        assert_eq!(response_json(&response)?["error"]["kind"], "usage");
    }

    // A cookie is accepted wherever it appears in the header.
    let cookie = format!("theme=dark; firestone_session={token}; consent=1");
    let response = tcp_get(port, "/v1/version", &[("Cookie", cookie.as_str())])?;
    assert_eq!(response_status(&response)?, 200);

    // The bootstrap moves the token out of the URL bar.
    let bootstrap = tcp_get(port, &format!("/?token={token}"), &[])?;
    assert_eq!(response_status(&bootstrap)?, 303);
    assert_eq!(
        response_header(&bootstrap, "location")?.as_deref(),
        Some("/")
    );
    assert_eq!(
        response_header(&bootstrap, "set-cookie")?.as_deref(),
        Some(
            format!("firestone_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400")
                .as_str()
        )
    );
    let wrong_bootstrap = tcp_get(port, &format!("/?token={}", "0".repeat(64)), &[])?;
    assert_eq!(response_status(&wrong_bootstrap)?, 401);
    assert_eq!(response_header(&wrong_bootstrap, "set-cookie")?, None);

    // Cross-origin mutation defense.
    let cross = tcp_request(
        port,
        "POST",
        "/v1/machines",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Sec-Fetch-Site", "cross-site"),
            ("Content-Type", "application/json"),
        ],
        br#"{"name":"demo","spec":{"image":"ubuntu:24.04"}}"#,
    )?;
    assert_eq!(response_status(&cross)?, 403);

    let foreign = tcp_request(
        port,
        "POST",
        "/v1/machines",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Origin", "http://evil.example.com"),
            ("Content-Type", "application/json"),
        ],
        br#"{"name":"demo","spec":{"image":"ubuntu:24.04"}}"#,
    )?;
    assert_eq!(response_status(&foreign)?, 403);

    let same_origin = tcp_request(
        port,
        "POST",
        "/v1/machines",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
        ],
        br#"{"name":"demo","spec":{"image":"ubuntu:24.04"}}"#,
    )?;
    assert_eq!(response_status(&same_origin)?, 201);

    assert!(server.stop()?.status.success());
    Ok(())
}

#[test]
fn ui_json_mode_announces_one_machine_readable_record() -> TestResult {
    let fixture = Fixture::new()?;
    // The human form is two lines; the JSON form is exactly one record.
    let server = UiServer::spawn(&fixture, &["--json"], 1)?;
    let record: serde_json::Value = serde_json::from_str(server.lines[0].trim_end())?;
    assert_eq!(
        record,
        serde_json::json!({
            "url": format!("http://127.0.0.1:{}/?token={}", server.port, server.token),
            "address": format!("127.0.0.1:{}", server.port),
            "port": server.port,
        })
    );
    assert!(server.stop()?.status.success());
    Ok(())
}

#[test]
fn ui_rejects_the_global_yes_flag_before_binding() -> TestResult {
    let fixture = Fixture::new()?;
    let mut yes = fixture.command();
    yes.args(["--json", "--yes", "ui", "--print-url"]);
    let yes = run_bounded(yes, COMMAND_TIMEOUT)?;
    assert_eq!(yes.status.code(), Some(2));
    assert!(yes.stderr.is_empty());
    assert_clean_text(&yes.stdout);
    let error: serde_json::Value = serde_json::from_slice(&yes.stdout)?;
    assert_eq!(error["error"]["kind"], "usage");
    assert_eq!(
        error["error"]["message"],
        "--yes is not valid with firestone ui"
    );
    Ok(())
}

fn free_loopback_port() -> TestResult<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn wait_for_tcp(port: u16, token: &str, timeout: Duration) -> TestResult<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = tcp_get(
            port,
            "/v1/machines",
            &[("Authorization", &format!("Bearer {token}"))],
        ) {
            if response_status(&response).ok() == Some(200) {
                return Ok(response);
            }
        }
        if Instant::now() >= deadline {
            return Err("serve did not answer on loopback before the deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn serve_reserved_tcp_form_creates_a_private_token_file_and_gates_requests() -> TestResult {
    let fixture = Fixture::new()?;
    let port = free_loopback_port()?;
    let token_file = fixture.home.join("serve.token");
    let listen = format!("tcp:127.0.0.1:{port}");
    let mut command = fixture.command();
    command
        .args(["serve", "--listen", &listen, "--token"])
        .arg(&token_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;

    wait_for_file(&token_file, START_TIMEOUT)?;
    let metadata = fs::symlink_metadata(&token_file)?;
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), getuid().as_raw());
    let token = fs::read_to_string(&token_file)?.trim().to_owned();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));

    wait_for_tcp(port, &token, START_TIMEOUT)?;

    let absent = tcp_get(port, "/v1/machines", &[])?;
    assert_eq!(response_status(&absent)?, 401);

    // The published Unix socket is untouched by a TCP listener.
    assert!(!fixture.socket.exists());

    kill(Pid::from_raw(i32::try_from(child.id())?), Signal::SIGTERM)?;
    let status = child
        .wait_timeout(STOP_TIMEOUT)?
        .ok_or("serve did not exit after SIGTERM")?;
    assert!(status.success());
    let output = child.wait_with_output()?;
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(&token),
        "the token must never be printed"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(&token),
        "the token must never be logged"
    );
    Ok(())
}
