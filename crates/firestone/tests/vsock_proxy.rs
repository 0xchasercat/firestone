use std::{
    error::Error,
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use firestone_core::{VSOCK_HANDSHAKE_MAX_BYTES, VSOCK_HANDSHAKE_TIMEOUT};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use wait_timeout::ChildExt;

type TestError = Box<dyn Error + Send + Sync>;
type TestResult = Result<(), TestError>;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self, TestError> {
        let temp = tempfile::tempdir()?;
        let root = fs::canonicalize(temp.path())?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let home = root.join("home");
        let runtime = home.join("run");
        for directory in [&home, &runtime] {
            fs::create_dir(directory)?;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            _temp: temp,
            root,
            home,
            runtime,
        })
    }

    fn listener(&self, name: &str) -> Result<UnixListener, TestError> {
        let machine_runtime = self.runtime.join(name);
        fs::create_dir(&machine_runtime)?;
        fs::set_permissions(&machine_runtime, fs::Permissions::from_mode(0o700))?;
        let socket = machine_runtime.join("vsock.sock");
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
        command
            .arg("--home")
            .arg(&self.home)
            .args(["_vsock-proxy", "demo", "22"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn accept_handshake(listener: UnixListener) -> Result<UnixStream, TestError> {
    let (mut stream, _) = listener.accept()?;
    let mut request = [0_u8; 11];
    stream.read_exact(&mut request)?;
    if request != *b"CONNECT 22\n" {
        return Err(format!("unexpected handshake: {request:?}").into());
    }
    Ok(stream)
}

fn collect_output(mut child: Child, timeout: Duration) -> Result<Output, TestError> {
    let stdout = child.stdout.take().ok_or("child stdout was not piped")?;
    let stderr = child.stderr.take().ok_or("child stderr was not piped")?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(u64::MAX).read_to_end(&mut bytes);
        result.map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(u64::MAX).read_to_end(&mut bytes);
        result.map(|_| bytes)
    });
    let status = match child.wait_timeout(timeout)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("proxy subprocess exceeded {} ms", timeout.as_millis()).into());
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "stdout reader panicked")??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "stderr reader panicked")??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn run_handshake_failure(response: Vec<u8>) -> Result<Output, TestError> {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        let _ = stream.write_all(&response);
        Ok(())
    });
    let mut child = fixture.command().spawn()?;
    drop(child.stdin.take());
    let output = collect_output(child, PROCESS_TIMEOUT)?;
    server.join().map_err(|_| "server panicked")??;
    Ok(output)
}

#[test]
fn proxy_malformed_oversize_and_partial_handshakes_fail_without_payload() -> TestResult {
    let malformed = run_handshake_failure(b"NO refused\n".to_vec())?;
    assert_eq!(malformed.status.code(), Some(1));
    assert!(malformed.stdout.is_empty());
    let malformed_stderr = String::from_utf8(malformed.stderr)?;
    assert!(malformed_stderr.contains("NO refused"));
    assert!(!malformed_stderr.contains("Result"));

    let oversized = run_handshake_failure(vec![b'x'; VSOCK_HANDSHAKE_MAX_BYTES + 1])?;
    assert_eq!(oversized.status.code(), Some(1));
    assert!(oversized.stdout.is_empty());
    assert!(String::from_utf8(oversized.stderr)?.contains("exceeds 64 bytes"));

    let partial = run_handshake_failure(b"OK 1073741824".to_vec())?;
    assert_eq!(partial.status.code(), Some(1));
    assert!(partial.stdout.is_empty());
    assert!(String::from_utf8(partial.stderr)?.contains("before a complete response line"));
    Ok(())
}

#[test]
fn proxy_partial_handshake_times_out_at_one_absolute_bound() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let (release, wait_for_release) = mpsc::channel();
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK ")?;
        let _ = wait_for_release.recv_timeout(Duration::from_secs(8));
        Ok(())
    });
    let mut child = fixture.command().spawn()?;
    drop(child.stdin.take());
    let started = Instant::now();
    let output = collect_output(child, VSOCK_HANDSHAKE_TIMEOUT + Duration::from_secs(3))?;
    let elapsed = started.elapsed();
    let _ = release.send(());
    server.join().map_err(|_| "server panicked")??;

    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)?.contains("timed out"));
    assert!(elapsed >= VSOCK_HANDSHAKE_TIMEOUT);
    assert!(elapsed < VSOCK_HANDSHAKE_TIMEOUT + Duration::from_secs(2));
    Ok(())
}

#[test]
fn proxy_stdin_eof_half_closes_and_preserves_binary_bytes_both_directions() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let input = b"\x00request\xff\n\x01".to_vec();
    let expected_input = input.clone();
    let response = b"\xfeanswer\x00\n\x7f".to_vec();
    let expected_response = response.clone();
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK 1073741824\n")?;
        let mut received = Vec::new();
        stream.read_to_end(&mut received)?;
        if received != expected_input {
            return Err(format!("binary request mismatch: {received:?}").into());
        }
        stream.write_all(&response)?;
        Ok(())
    });
    let mut child = fixture.command().spawn()?;
    let mut stdin = child.stdin.take().ok_or("child stdin was not piped")?;
    stdin.write_all(&input)?;
    drop(stdin);
    let output = collect_output(child, PROCESS_TIMEOUT)?;
    server.join().map_err(|_| "server panicked")??;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, expected_response);
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn proxy_socket_eof_exits_while_stdin_remains_open() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK 1073741824\n")?;
        Ok(())
    });
    let mut child = fixture.command().spawn()?;
    let open_stdin = child.stdin.take().ok_or("child stdin was not piped")?;
    let started = Instant::now();
    let output = collect_output(child, Duration::from_secs(3))?;
    drop(open_stdin);
    server.join().map_err(|_| "server panicked")??;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
    Ok(())
}

#[test]
fn proxy_flushes_server_bytes_before_socket_eof() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let response = b"server-first-without-newline".to_vec();
    let expected_response = response.clone();
    let response_len = expected_response.len();
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK 1073741824\n")?;
        stream.write_all(&response)?;
        let mut request = [0_u8; 1];
        stream.read_exact(&mut request)?;
        if request != *b"x" {
            return Err(format!("unexpected request byte: {request:?}").into());
        }
        Ok(())
    });
    let mut child = fixture.command().spawn()?;
    let mut stdin = child.stdin.take().ok_or("child stdin was not piped")?;
    let mut stdout = child.stdout.take().ok_or("child stdout was not piped")?;
    let (progress_sender, progress_receiver) = mpsc::channel();
    let output_reader = thread::spawn(move || {
        let mut prefix = vec![0_u8; response_len];
        let progress = stdout
            .read_exact(&mut prefix)
            .map(|()| prefix.clone())
            .map_err(|error| error.to_string());
        let _ = progress_sender.send(progress);
        let mut remainder = Vec::new();
        let result = stdout.read_to_end(&mut remainder);
        result.map(|_| {
            prefix.extend(remainder);
            prefix
        })
    });

    let progress = match progress_receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(progress) => progress.map_err(|error| format!("proxy stdout failed: {error}"))?,
        Err(error) => {
            let _ = child.kill();
            drop(stdin);
            let _ = child.wait();
            let _ = server.join();
            let _ = output_reader.join();
            return Err(format!("proxy did not flush server bytes before EOF: {error}").into());
        }
    };
    assert_eq!(progress, expected_response);
    stdin.write_all(b"x")?;
    drop(stdin);
    let status = child
        .wait_timeout(PROCESS_TIMEOUT)?
        .ok_or("proxy did not exit after the server closed")?;
    let stdout = output_reader
        .join()
        .map_err(|_| "stdout reader panicked")??;
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .ok_or("child stderr was not piped")?
        .read_to_end(&mut stderr)?;
    server.join().map_err(|_| "server panicked")??;

    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    assert_eq!(stdout, expected_response);
    assert!(stderr.is_empty());
    Ok(())
}
#[test]
fn proxy_relays_backpressured_binary_streams_losslessly() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let input = (0..2 * 1024 * 1024)
        .map(|index| ((index * 31) % 251) as u8)
        .collect::<Vec<_>>();
    let response = (0..2 * 1024 * 1024)
        .map(|index| (255 - ((index * 17) % 251)) as u8)
        .collect::<Vec<_>>();
    let expected_input = input.clone();
    let expected_response = response.clone();
    let server = thread::spawn(move || -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK 1073741824\n")?;
        let mut writer = stream.try_clone()?;
        let sender = thread::spawn(move || writer.write_all(&response));
        let mut received = Vec::new();
        stream.read_to_end(&mut received)?;
        sender.join().map_err(|_| "server writer panicked")??;
        Ok(received)
    });
    let mut child = fixture.command().spawn()?;
    let mut stdin = child.stdin.take().ok_or("child stdin was not piped")?;
    let input_writer = thread::spawn(move || stdin.write_all(&input));
    let output = collect_output(child, PROCESS_TIMEOUT)?;
    input_writer.join().map_err(|_| "stdin writer panicked")??;
    let received = server.join().map_err(|_| "server panicked")??;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(received, expected_input);
    assert_eq!(output.stdout, expected_response);
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn proxy_sigterm_cancels_blocked_relay_promptly() -> TestResult {
    let fixture = Fixture::new()?;
    let listener = fixture.listener("demo")?;
    let (ready, wait_until_ready) = mpsc::channel();
    let (release, wait_for_release) = mpsc::channel();
    let server = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut stream = accept_handshake(listener)?;
        stream.write_all(b"OK 1073741824\n")?;
        ready.send(())?;
        let _ = wait_for_release.recv_timeout(Duration::from_secs(5));
        Ok(())
    });
    let child = fixture.command().spawn()?;
    wait_until_ready.recv_timeout(Duration::from_secs(2))?;
    let pid = i32::try_from(child.id())?;
    let started = Instant::now();
    kill(Pid::from_raw(pid), Signal::SIGTERM)?;
    let output = collect_output(child, Duration::from_secs(2))?;
    let _ = release.send(());
    server.join().map_err(|_| "server panicked")??;

    assert_eq!(output.status.signal(), Some(Signal::SIGTERM as i32));
    assert!(started.elapsed() < Duration::from_secs(1));
    Ok(())
}

fn command_output(home: &Path, arguments: &[&str]) -> Result<Output, TestError> {
    Ok(Command::new(env!("CARGO_BIN_EXE_firestone"))
        .arg("--home")
        .arg(home)
        .args(arguments)
        .output()?)
}

fn assert_exit(status: ExitStatus, code: i32) {
    assert_eq!(status.code(), Some(code), "unexpected status {status}");
}

#[test]
fn proxy_validates_name_port_runtime_socket_paths_and_modes() -> TestResult {
    let missing = Fixture::new()?;
    let output = command_output(&missing.home, &["_vsock-proxy", "demo", "22"])?;
    assert_exit(output.status, 1);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("not_running"));
    assert!(stderr.contains("start machine `demo`"));
    assert!(output.stdout.is_empty());

    for invalid_port in ["0", "4294967296", "nope"] {
        let output = command_output(&missing.home, &["_vsock-proxy", "demo", invalid_port])?;
        assert_exit(output.status, 2);
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)?.contains("vsock port must be between"));
    }
    let output = command_output(&missing.home, &["_vsock-proxy", "../demo", "22"])?;
    assert_exit(output.status, 2);
    assert!(String::from_utf8(output.stderr)?.contains("invalid_spec"));

    let bad_runtime = Fixture::new()?;
    let listener = bad_runtime.listener("demo")?;
    fs::set_permissions(
        bad_runtime.runtime.join("demo"),
        fs::Permissions::from_mode(0o755),
    )?;
    let output = command_output(&bad_runtime.home, &["_vsock-proxy", "demo", "22"])?;
    assert_exit(output.status, 5);
    assert!(String::from_utf8(output.stderr)?.contains("mode 0755"));
    drop(listener);

    let regular = Fixture::new()?;
    let machine_runtime = regular.runtime.join("demo");
    fs::create_dir(&machine_runtime)?;
    fs::set_permissions(&machine_runtime, fs::Permissions::from_mode(0o700))?;
    fs::write(machine_runtime.join("vsock.sock"), b"not-a-socket")?;
    fs::set_permissions(
        machine_runtime.join("vsock.sock"),
        fs::Permissions::from_mode(0o600),
    )?;
    let output = command_output(&regular.home, &["_vsock-proxy", "demo", "22"])?;
    assert_exit(output.status, 5);
    assert!(
        String::from_utf8(output.stderr)?.contains("expected a current-user protected Unix socket")
    );

    let linked = Fixture::new()?;
    let machine_runtime = linked.runtime.join("demo");
    fs::create_dir(&machine_runtime)?;
    fs::set_permissions(&machine_runtime, fs::Permissions::from_mode(0o700))?;
    let outside = linked.root.join("outside.sock");
    let outside_listener = UnixListener::bind(&outside)?;
    symlink(&outside, machine_runtime.join("vsock.sock"))?;
    let output = command_output(&linked.home, &["_vsock-proxy", "demo", "22"])?;
    assert_exit(output.status, 5);
    assert!(outside.exists());
    assert!(
        String::from_utf8(output.stderr)?.contains("expected a current-user protected Unix socket")
    );
    drop(outside_listener);

    let broad = Fixture::new()?;
    let broad_listener = broad.listener("demo")?;
    fs::set_permissions(
        broad.runtime.join("demo/vsock.sock"),
        fs::Permissions::from_mode(0o622),
    )?;
    let output = command_output(&broad.home, &["_vsock-proxy", "demo", "22"])?;
    assert_exit(output.status, 5);
    assert!(String::from_utf8(output.stderr)?.contains("mode 0622"));
    drop(broad_listener);
    Ok(())
}

#[test]
fn proxy_command_is_hidden_from_normal_help() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .arg("--help")
        .output()?;
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout)?.contains("_vsock-proxy"));
    Ok(())
}
