use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, getuid},
};
use serde_json::{Value, json};
use wait_timeout::ChildExt as _;

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(8);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    socket: PathBuf,
    path: OsString,
    fake_vmm: PathBuf,
    source: PathBuf,
    second_source: PathBuf,
    firmware: PathBuf,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let directory = tempfile::tempdir()?;
        let root = fs::canonicalize(directory.path())?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(
            env!("CARGO_BIN_EXE_firestone"),
            fs::Permissions::from_mode(0o755),
        )?;

        let home = root.join("home");
        fs::create_dir(&home)?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        let socket = home.join("run/serve.sock");
        let fake_vmm = compile_fake_vmm(&root)?;
        let bin = root.join("bin");
        fs::create_dir(&bin)?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
        let qemu_img = bin.join("qemu-img");
        fs::copy(&fake_vmm, &qemu_img)?;
        fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))?;
        let mut path_entries = vec![bin];
        if let Some(existing) = env::var_os("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        let path = env::join_paths(path_entries)?;

        let source = root.join("base.qcow2");
        fs::write(&source, b"QFI\xfbM4-REST-EQUIVALENCE")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let second_source = root.join("second.qcow2");
        fs::write(&second_source, b"QFI\xfbM4-REST-EQUIVALENCE-SECOND")?;
        fs::set_permissions(&second_source, fs::Permissions::from_mode(0o600))?;
        let firmware = root.join("firmware.fd");
        fs::write(&firmware, b"firmware")?;
        fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            _directory: directory,
            root,
            home,
            socket,
            path,
            fake_vmm,
            source,
            second_source,
            firmware,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
        command
            .arg("--home")
            .arg(&self.home)
            .env("PATH", &self.path);
        command
    }

    fn json_command(&self, arguments: &[&str]) -> TestResult<Output> {
        let mut command = self.command();
        command.arg("--json").args(arguments);
        run_bounded(command, COMMAND_TIMEOUT)
    }

    fn create_fake_machine(
        &self,
        name: &str,
        behavior: &str,
        cpus: Option<u8>,
    ) -> TestResult<Output> {
        let machine_dir = self.home.join("data/machines").join(name);
        let record = self.root.join(format!("{name}-requests.log"));
        let body = self.root.join(format!("{name}-body.json"));
        let console = machine_dir.join("console.log");
        let mut command = self.command();
        command
            .arg("--json")
            .arg("create")
            .arg(name)
            .arg(&self.source)
            .args(["--net", "none", "--no-provisioning"])
            .arg("--vmm-binary")
            .arg(&self.fake_vmm)
            .arg("--vmm-firmware")
            .arg(&self.firmware);
        if let Some(cpus) = cpus {
            command.arg("--cpus").arg(cpus.to_string());
        }
        for value in [
            "--record".to_owned(),
            record.to_string_lossy().into_owned(),
            "--body".to_owned(),
            body.to_string_lossy().into_owned(),
            "--behavior".to_owned(),
            behavior.to_owned(),
            "--console-log".to_owned(),
            console.to_string_lossy().into_owned(),
        ] {
            command.arg(format!("--vmm-arg={value}"));
        }
        run_bounded(command, COMMAND_TIMEOUT)
    }

    fn remove_machine(&self, name: &str) {
        let _ = self.json_command(&["stop", name, "--force", "--timeout", "1s"]);
        let _ = self.json_command(&["rm", name, "--force"]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let machines = self.home.join("data/machines");
        let Ok(entries) = fs::read_dir(machines) else {
            return;
        };
        let names = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect::<Vec<_>>();
        for name in names {
            self.remove_machine(&name);
        }
    }
}

struct Server {
    child: Option<Child>,
    socket: PathBuf,
}

impl Server {
    fn spawn(fixture: &Fixture) -> TestResult<Self> {
        let mut command = fixture.command();
        command
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut server = Self {
            child: Some(command.spawn()?),
            socket: fixture.socket.clone(),
        };
        server.wait_until_ready()?;
        Ok(server)
    }

    fn wait_until_ready(&mut self) -> TestResult {
        let deadline = Instant::now() + SERVER_START_TIMEOUT;
        loop {
            match fs::symlink_metadata(&self.socket) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    assert_eq!(metadata.mode() & 0o7777, 0o600);
                    assert_eq!(metadata.uid(), getuid().as_raw());
                    if UnixStream::connect(&self.socket).is_ok() {
                        return Ok(());
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
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn child_mut(&mut self) -> TestResult<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| "serve process already collected".into())
    }

    fn pid(&self) -> TestResult<Pid> {
        let id = self
            .child
            .as_ref()
            .ok_or("serve process already collected")?
            .id();
        Ok(Pid::from_raw(i32::try_from(id)?))
    }

    fn signal(mut self, signal: Signal) -> TestResult<Output> {
        kill(self.pid()?, signal)?;
        let child = self.child_mut()?;
        if child.wait_timeout(SERVER_STOP_TIMEOUT)?.is_none() {
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

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn compile_fake_vmm(root: &Path) -> TestResult<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_vmm.rs");
    let binary = root.join("fake-vmm");
    let output = Command::new("rustc")
        .args(["--edition=2024", "-C", "debuginfo=0"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to compile fake VMM: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
    Ok(binary)
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

fn require_success(output: &Output, label: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{label} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn write_http_request(
    stream: &mut UnixStream,
    method: &str,
    path: &str,
    body: &[u8],
    accept: Option<&str>,
) -> TestResult {
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\n"
    )?;
    if !body.is_empty() {
        write!(stream, "Content-Type: application/json\r\n")?;
    }
    if let Some(accept) = accept {
        write!(stream, "Accept: {accept}\r\n")?;
    }
    write!(stream, "Content-Length: {}\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn open_http_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: &[u8],
    accept: Option<&str>,
) -> TestResult<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    write_http_request(&mut stream, method, path, body, accept)?;
    Ok(stream)
}

fn http_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: &[u8],
    accept: Option<&str>,
) -> TestResult<HttpResponse> {
    let mut stream = open_http_request(socket, method, path, body, accept)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_http_response(&response)
}

fn parse_http_response(response: &[u8]) -> TestResult<HttpResponse> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("HTTP response has no header terminator")?;
    let header_bytes = &response[..header_end];
    let header_text = std::str::from_utf8(header_bytes)?;
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("HTTP response has no status code")?
        .parse()?;
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or("HTTP response header is malformed")?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let wire_body = &response[header_end + 4..];
    let body = if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        decode_chunked(wire_body)?
    } else if let Some(length) = headers.get("content-length") {
        let length = length.parse::<usize>()?;
        if wire_body.len() < length {
            return Err("HTTP response body is shorter than Content-Length".into());
        }
        wire_body[..length].to_vec()
    } else {
        wire_body.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut input: &[u8]) -> TestResult<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("chunked body has no size terminator")?;
        let size_text = std::str::from_utf8(&input[..line_end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default(), 16)?;
        input = &input[line_end + 2..];
        if size == 0 {
            return Ok(output);
        }
        if input.len() < size + 2 || &input[size..size + 2] != b"\r\n" {
            return Err("chunked body has a truncated frame".into());
        }
        output.extend_from_slice(&input[..size]);
        input = &input[size + 2..];
    }
}

#[derive(Debug)]
struct TerminalResult {
    payload: Value,
    payload_bytes: Vec<u8>,
    records: Vec<Value>,
}

fn terminal_result(output: &[u8], expected_action: &str) -> TestResult<TerminalResult> {
    let lines = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Err(format!("{expected_action} emitted no NDJSON records").into());
    }
    let records = lines
        .iter()
        .map(|line| serde_json::from_slice::<Value>(line))
        .collect::<Result<Vec<_>, _>>()?;
    let terminal = records.last().ok_or("NDJSON has no terminal record")?;
    if terminal["type"] != "Result" || terminal["action"] != expected_action {
        return Err(format!("{expected_action} did not end in its Result: {terminal}").into());
    }
    let result_count = records
        .iter()
        .filter(|record| record["type"] == "Result")
        .count();
    if result_count != 1 {
        return Err(format!("{expected_action} emitted {result_count} Result records").into());
    }
    let line = lines.last().ok_or("NDJSON has no terminal bytes")?;
    let marker = b",\"payload\":";
    let start = line
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|index| index + marker.len())
        .ok_or("Result record has no payload field")?;
    if line.last() != Some(&b'}') {
        return Err("Result record does not end in an object delimiter".into());
    }
    let payload_bytes = line[start..line.len() - 1].to_vec();
    let payload: Value = serde_json::from_slice(&payload_bytes)?;
    if payload != terminal["payload"] {
        return Err("raw Result payload does not match its parsed value".into());
    }
    Ok(TerminalResult {
        payload,
        payload_bytes,
        records,
    })
}

fn assert_exact_payload(label: &str, cli: &TerminalResult, rest: &[u8]) -> TestResult {
    let rest_value: Value = serde_json::from_slice(rest)?;
    assert_eq!(rest_value, cli.payload, "{label} parsed payload changed");
    assert_eq!(rest, cli.payload_bytes, "{label} payload bytes changed");
    Ok(())
}

fn redact_elapsed(payload: &[u8]) -> TestResult<Vec<u8>> {
    let marker = b"\"elapsed_ms\":";
    let start = payload
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|index| index + marker.len())
        .ok_or("lifecycle Result has no elapsed_ms")?;
    let digits = payload[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return Err("lifecycle Result elapsed_ms is not an integer".into());
    }
    let mut redacted = payload[..start].to_vec();
    redacted.extend_from_slice(b"<elapsed_ms>");
    redacted.extend_from_slice(&payload[start + digits..]);
    Ok(redacted)
}

fn assert_lifecycle_payload(
    label: &str,
    cli: &TerminalResult,
    rest: &TerminalResult,
) -> TestResult {
    let cli_elapsed = cli.payload["elapsed_ms"]
        .as_u64()
        .ok_or("CLI lifecycle elapsed_ms is not a u64")?;
    let rest_elapsed = rest.payload["elapsed_ms"]
        .as_u64()
        .ok_or("REST lifecycle elapsed_ms is not a u64")?;
    assert_eq!(
        redact_elapsed(&cli.payload_bytes)?,
        redact_elapsed(&rest.payload_bytes)?,
        "{label} stable payload bytes or field order changed; elapsed values were CLI={cli_elapsed}, REST={rest_elapsed}"
    );
    Ok(())
}

fn redact_doctor_free_bytes(payload: &[u8]) -> TestResult<Vec<u8>> {
    let marker = b"data filesystem has ";
    let start = payload
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|index| index + marker.len())
        .ok_or("doctor Result has no data-space check")?;
    let digits = payload[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return Err("doctor data-space free bytes is not an integer".into());
    }
    let mut redacted = payload[..start].to_vec();
    redacted.extend_from_slice(b"<free_bytes>");
    redacted.extend_from_slice(&payload[start + digits..]);
    Ok(redacted)
}

fn normalize_doctor_free_bytes(payload: &mut Value) -> TestResult<u64> {
    let checks = payload["checks"]
        .as_array_mut()
        .ok_or("doctor Result checks is not an array")?;
    let check = checks
        .iter_mut()
        .find(|check| check["id"] == "data_space")
        .ok_or("doctor Result has no data_space check")?;
    let reason = check["reason"]
        .as_str()
        .ok_or("doctor data_space reason is not a string")?;
    let free = reason
        .strip_prefix("data filesystem has ")
        .and_then(|value| value.split_once(" bytes free"))
        .map(|(value, _)| value)
        .ok_or("doctor data_space reason has unexpected framing")?
        .parse::<u64>()?;
    check["reason"] = Value::String(reason.replacen(&free.to_string(), "<free_bytes>", 1));
    Ok(free)
}

fn assert_doctor_payload(cli: &TerminalResult, rest: &[u8]) -> TestResult {
    let mut cli_value = cli.payload.clone();
    let mut rest_value: Value = serde_json::from_slice(rest)?;
    let cli_free = normalize_doctor_free_bytes(&mut cli_value)?;
    let rest_free = normalize_doctor_free_bytes(&mut rest_value)?;
    assert!(cli_free > 0 && rest_free > 0);
    assert_eq!(rest_value, cli_value, "doctor stable fields changed");
    assert_eq!(
        redact_doctor_free_bytes(rest)?,
        redact_doctor_free_bytes(&cli.payload_bytes)?,
        "doctor stable payload bytes or field order changed"
    );
    Ok(())
}

fn error_bytes(output: &[u8]) -> TestResult<Vec<u8>> {
    let lines = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!("expected one error record, got {}", lines.len()).into());
    }
    let value: Value = serde_json::from_slice(lines[0])?;
    if !value["error"].is_object() {
        return Err("terminal error record has no error object".into());
    }
    Ok(lines[0].to_vec())
}

fn create_body(name: &str, spec: &Value) -> TestResult<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({"name": name, "spec": spec}))?)
}

fn rest_create(socket: &Path, name: &str, spec: &Value) -> TestResult<HttpResponse> {
    let body = create_body(name, spec)?;
    http_request(socket, "POST", "/v1/machines", &body, None)
}

fn wait_for_state(home: &Path, name: &str, expected: &str, timeout: Duration) -> TestResult<Value> {
    let path = home.join("data/machines").join(name).join("state.json");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(state) = serde_json::from_slice::<Value>(&bytes) {
                if state["status"] == expected {
                    return Ok(state);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("{name} did not reach state {expected}").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_list_status(
    socket: &Path,
    name: &str,
    expected: &str,
    timeout: Duration,
) -> TestResult<Value> {
    let deadline = Instant::now() + timeout;
    let mut last = Value::Null;
    loop {
        let response = http_request(socket, "GET", "/v1/machines", b"", None)?;
        if response.status == 200 {
            last = serde_json::from_slice(&response.body)?;
            if last.as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["name"] == name && row["status"] == expected)
            }) {
                return Ok(last);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{name} did not reach list status {expected}; last response was {last}"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_until(stream: &mut UnixStream, marker: &[u8], timeout: Duration) -> TestResult<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    let deadline = Instant::now() + timeout;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Err("HTTP stream ended before the expected marker".into()),
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response
                    .windows(marker.len())
                    .any(|window| window == marker)
                {
                    return Ok(response);
                }
            }
            Err(source)
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(source) => return Err(source.into()),
        }
        if Instant::now() >= deadline {
            return Err("HTTP stream did not deliver the expected marker in time".into());
        }
    }
}

#[test]
fn real_cli_and_unix_rest_project_stable_result_payload_bytes() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;

    let missing_cli = fixture.json_command(&["show", "absent"])?;
    assert_eq!(missing_cli.status.code(), Some(3));
    let missing_rest = http_request(&server.socket, "GET", "/v1/machines/absent", b"", None)?;
    assert_eq!(missing_rest.status, 404);
    assert_eq!(missing_rest.body, error_bytes(&missing_cli.stdout)?);

    let version_cli = fixture.json_command(&["version"])?;
    require_success(&version_cli, "CLI version")?;
    let version_cli = terminal_result(&version_cli.stdout, "version")?;
    let version_rest = http_request(&server.socket, "GET", "/v1/version", b"", None)?;
    assert_eq!(version_rest.status, 200);
    assert_exact_payload("version", &version_cli, &version_rest.body)?;

    let doctor_cli = fixture.json_command(&["doctor"])?;
    assert_eq!(doctor_cli.status.code(), Some(5));
    let doctor_cli = terminal_result(&doctor_cli.stdout, "doctor")?;
    let doctor_rest = http_request(&server.socket, "GET", "/v1/doctor", b"", None)?;
    assert_eq!(doctor_rest.status, 200);
    assert_doctor_payload(&doctor_cli, &doctor_rest.body)?;

    let created_cli = fixture.create_fake_machine("equiv", "normal", None)?;
    require_success(&created_cli, "CLI create")?;
    let created_cli = terminal_result(&created_cli.stdout, "create")?;
    let initial_spec = created_cli.payload["spec"].clone();
    let removed = fixture.json_command(&["rm", "equiv"])?;
    require_success(&removed, "remove after CLI create")?;

    let created_rest = rest_create(&server.socket, "equiv", &initial_spec)?;
    assert_eq!(created_rest.status, 201);
    assert_exact_payload("create", &created_cli, &created_rest.body)?;

    let show_cli = fixture.json_command(&["show", "equiv"])?;
    require_success(&show_cli, "CLI show")?;
    let show_cli = terminal_result(&show_cli.stdout, "show")?;
    let show_rest = http_request(&server.socket, "GET", "/v1/machines/equiv", b"", None)?;
    assert_eq!(show_rest.status, 200);
    assert_exact_payload("show", &show_cli, &show_rest.body)?;

    let list_cli = fixture.json_command(&["ls"])?;
    require_success(&list_cli, "CLI list")?;
    let list_cli = terminal_result(&list_cli.stdout, "list")?;
    let list_rest = http_request(&server.socket, "GET", "/v1/machines", b"", None)?;
    assert_eq!(list_rest.status, 200);
    assert_exact_payload("list", &list_cli, &list_rest.body)?;

    let original_toml = fs::read(fixture.home.join("data/machines/equiv/firestone.toml"))?;
    let original_text = std::str::from_utf8(&original_toml)?;
    let desired_text = original_text.replacen("cpus = 2", "cpus = 3", 1);
    if desired_text == original_text {
        return Err("machine spec did not contain the default CPU line".into());
    }
    let desired_toml = desired_text.into_bytes();

    let editor = fixture.root.join("replace-editor.sh");
    fs::write(
        &editor,
        b"#!/bin/sh\nset -eu\ncp \"$FIRESTONE_EDIT_SOURCE\" \"$1\"\n",
    )?;
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700))?;
    let desired_path = fixture.root.join("desired.toml");
    fs::write(&desired_path, &desired_toml)?;
    fs::set_permissions(&desired_path, fs::Permissions::from_mode(0o600))?;
    let mut edit = fixture.command();
    edit.args(["--json", "edit", "equiv"])
        .env("VISUAL", format!("sh {}", editor.display()))
        .env("FIRESTONE_EDIT_SOURCE", &desired_path);
    let edited_cli = run_bounded(edit, COMMAND_TIMEOUT)?;
    require_success(&edited_cli, "CLI edit")?;
    let edited_cli = terminal_result(&edited_cli.stdout, "edit")?;
    let desired_spec = edited_cli.payload["spec"].clone();
    assert_eq!(desired_spec["cpus"], 3);

    fs::write(
        fixture.home.join("data/machines/equiv/firestone.toml"),
        &original_toml,
    )?;
    let put_body = serde_json::to_vec(&desired_spec)?;
    let put_rest = http_request(&server.socket, "PUT", "/v1/machines/equiv", &put_body, None)?;
    assert_eq!(put_rest.status, 200);
    assert_exact_payload("PUT edit", &edited_cli, &put_rest.body)?;

    fs::write(
        fixture.home.join("data/machines/equiv/firestone.toml"),
        &original_toml,
    )?;
    let patch_rest = http_request(
        &server.socket,
        "PATCH",
        "/v1/machines/equiv",
        br#"{"cpus":3}"#,
        None,
    )?;
    assert_eq!(patch_rest.status, 200);
    assert_exact_payload("PATCH edit", &edited_cli, &patch_rest.body)?;

    let second = fixture.second_source.to_string_lossy().into_owned();
    let seed_pull = fixture.json_command(&["images", "pull", &second])?;
    require_success(&seed_pull, "seed image pull")?;
    let pull_cli = fixture.json_command(&["images", "pull", &second])?;
    require_success(&pull_cli, "cached CLI image pull")?;
    let pull_cli = terminal_result(&pull_cli.stdout, "images-pull")?;
    assert_eq!(pull_cli.payload["cached"], true);
    let pull_body = serde_json::to_vec(&json!({"ref": second}))?;
    let pull_rest = http_request(&server.socket, "POST", "/v1/images/pull", &pull_body, None)?;
    assert_eq!(pull_rest.status, 200);
    let pull_rest = terminal_result(&pull_rest.body, "images-pull")?;
    assert_eq!(pull_rest.payload_bytes, pull_cli.payload_bytes);

    let images_cli = fixture.json_command(&["images", "ls"])?;
    require_success(&images_cli, "CLI image list")?;
    let images_cli = terminal_result(&images_cli.stdout, "images-ls")?;
    let images_rest = http_request(&server.socket, "GET", "/v1/images", b"", None)?;
    assert_eq!(images_rest.status, 200);
    assert_exact_payload("image list", &images_cli, &images_rest.body)?;

    let prune_cli = fixture.json_command(&["images", "prune"])?;
    require_success(&prune_cli, "CLI image prune")?;
    let prune_cli = terminal_result(&prune_cli.stdout, "images-prune")?;
    require_success(
        &fixture.json_command(&["images", "pull", &second])?,
        "reseed image before REST prune",
    )?;
    let prune_rest = http_request(&server.socket, "POST", "/v1/images/prune", b"", None)?;
    assert_eq!(prune_rest.status, 200);
    assert_exact_payload("image prune", &prune_cli, &prune_rest.body)?;

    let pulled = fixture.json_command(&["images", "pull", &second])?;
    require_success(&pulled, "image pull before remove")?;
    let pulled = terminal_result(&pulled.stdout, "images-pull")?;
    let image_id = pulled.payload["metadata"]["id"]
        .as_str()
        .ok_or("pulled image has no id")?
        .to_owned();
    let image_remove_cli = fixture.json_command(&["images", "rm", &image_id])?;
    require_success(&image_remove_cli, "CLI image remove")?;
    let image_remove_cli = terminal_result(&image_remove_cli.stdout, "images-rm")?;
    assert_eq!(image_remove_cli.payload["id"], image_id);
    require_success(
        &fixture.json_command(&["images", "pull", &second])?,
        "reseed image before REST remove",
    )?;
    let image_remove_rest = http_request(
        &server.socket,
        "DELETE",
        &format!("/v1/images/{image_id}"),
        b"",
        None,
    )?;
    assert_eq!(image_remove_rest.status, 204);
    assert!(image_remove_rest.body.is_empty());

    let start_cli = fixture.json_command(&["start", "equiv", "--no-wait", "--timeout", "20s"])?;
    require_success(&start_cli, "CLI start")?;
    let start_cli = terminal_result(&start_cli.stdout, "start")?;

    let vmconfig_cli = fixture.json_command(&["show", "equiv", "--vmconfig"])?;
    require_success(&vmconfig_cli, "CLI vmconfig")?;
    let vmconfig_cli = terminal_result(&vmconfig_cli.stdout, "show-vmconfig")?;
    let vmconfig_rest = http_request(
        &server.socket,
        "GET",
        "/v1/machines/equiv/vmconfig",
        b"",
        None,
    )?;
    assert_eq!(vmconfig_rest.status, 200);
    assert_exact_payload("vmconfig", &vmconfig_cli, &vmconfig_rest.body)?;

    let logs_rest = http_request(
        &server.socket,
        "GET",
        "/v1/machines/equiv/logs?source=console&lines=1",
        b"",
        None,
    )?;
    assert_eq!(logs_rest.status, 200);
    assert_eq!(
        logs_rest.headers.get("content-type").map(String::as_str),
        Some("text/plain")
    );
    assert_eq!(logs_rest.body, b"current boot\n");

    let running_cli = fixture.json_command(&["start", "equiv", "--timeout", "2s"])?;
    assert_eq!(running_cli.status.code(), Some(4));
    let running_rest = http_request(
        &server.socket,
        "POST",
        "/v1/machines/equiv/start",
        br#"{"timeout_s":2}"#,
        None,
    )?;
    assert_eq!(running_rest.status, 409);
    let mut running_rest_error = running_rest.body.clone();
    if running_rest_error.last() == Some(&b'\n') {
        running_rest_error.pop();
    }
    assert_eq!(running_rest_error, error_bytes(&running_cli.stdout)?);

    let stop_cli = fixture.json_command(&["stop", "equiv", "--timeout", "5s"])?;
    require_success(&stop_cli, "CLI stop")?;
    let stop_cli = terminal_result(&stop_cli.stdout, "stop")?;
    require_success(
        &fixture.json_command(&["rm", "equiv"])?,
        "remove before REST start",
    )?;

    let created = rest_create(&server.socket, "equiv", &desired_spec)?;
    assert_eq!(created.status, 201);
    let start_rest = http_request(
        &server.socket,
        "POST",
        "/v1/machines/equiv/start",
        br#"{"wait":false,"timeout_s":20}"#,
        None,
    )?;
    assert_eq!(start_rest.status, 200);
    let start_rest = terminal_result(&start_rest.body, "start")
        .map_err(|error| format!("REST start equivalence failed: {error}"))?;
    assert_lifecycle_payload("start", &start_cli, &start_rest)?;

    let stop_rest = http_request(
        &server.socket,
        "POST",
        "/v1/machines/equiv/stop",
        br#"{"timeout_s":5}"#,
        None,
    )?;
    assert_eq!(stop_rest.status, 200);
    let stop_rest = terminal_result(&stop_rest.body, "stop")?;
    assert_lifecycle_payload("stop", &stop_cli, &stop_rest)?;

    require_success(
        &fixture.json_command(&["start", "equiv", "--no-wait", "--timeout", "20s"])?,
        "start before CLI restart",
    )?;
    let restart_cli = fixture.json_command(&["restart", "equiv"])?;
    require_success(&restart_cli, "CLI restart")?;
    let restart_cli = terminal_result(&restart_cli.stdout, "restart")?;
    let remove_cli = fixture.json_command(&["rm", "equiv", "--force"])?;
    require_success(&remove_cli, "CLI running remove")?;
    let remove_cli = terminal_result(&remove_cli.stdout, "rm")?;

    assert_eq!(
        rest_create(&server.socket, "equiv", &desired_spec)?.status,
        201
    );
    let restart_start = http_request(
        &server.socket,
        "POST",
        "/v1/machines/equiv/start",
        br#"{"wait":false,"timeout_s":20}"#,
        None,
    )?;
    assert_eq!(restart_start.status, 200);
    terminal_result(&restart_start.body, "start")
        .map_err(|error| format!("REST setup start before restart failed: {error}"))?;
    let restart_rest = http_request(
        &server.socket,
        "POST",
        "/v1/machines/equiv/restart",
        b"",
        None,
    )?;
    assert_eq!(restart_rest.status, 200);
    let restart_rest = terminal_result(&restart_rest.body, "restart")
        .map_err(|error| format!("REST restart equivalence failed: {error}"))?;
    assert_lifecycle_payload("restart", &restart_cli, &restart_rest)?;

    let remove_rest = http_request(
        &server.socket,
        "DELETE",
        "/v1/machines/equiv?force=true",
        b"",
        None,
    )?;
    assert_eq!(remove_rest.status, 200);
    let remove_rest = terminal_result(&remove_rest.body, "rm")?;
    assert_eq!(remove_rest.payload_bytes, remove_cli.payload_bytes);

    assert_eq!(
        rest_create(&server.socket, "delete204", &desired_spec)?.status,
        201
    );
    let remove_204 = http_request(
        &server.socket,
        "DELETE",
        "/v1/machines/delete204",
        b"",
        None,
    )?;
    assert_eq!(remove_204.status, 204);
    assert!(remove_204.body.is_empty());

    let output = server.signal(Signal::SIGTERM)?;
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn real_unix_rest_streams_aggregates_disconnects_and_survives_restart() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;

    let delayed = fixture.create_fake_machine("delayed", "delayed-ready", None)?;
    require_success(&delayed, "delayed machine create")?;
    let mut stream = open_http_request(
        &server.socket,
        "POST",
        "/v1/machines/delayed/start",
        br#"{"wait":false,"timeout_s":20}"#,
        None,
    )?;
    let started = Instant::now();
    let mut response = read_until(
        &mut stream,
        b"\"type\":\"StepStart\"",
        Duration::from_millis(700),
    )?;
    assert!(started.elapsed() < Duration::from_millis(700));
    assert!(
        !response
            .windows(17)
            .any(|window| window == b"\"type\":\"Result\"")
    );
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.read_to_end(&mut response)?;
    let response = parse_http_response(&response)?;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("application/x-ndjson")
    );
    let streamed = terminal_result(&response.body, "start")?;
    assert_eq!(
        streamed.records.last().map(|value| &value["type"]),
        Some(&json!("Result"))
    );

    let logs = http_request(
        &server.socket,
        "GET",
        "/v1/machines/delayed/logs?lines=1",
        b"",
        None,
    )?;
    assert_eq!(logs.status, 200);
    assert_eq!(
        logs.headers.get("content-type").map(String::as_str),
        Some("text/plain")
    );
    assert_eq!(logs.body, b"current boot\n");
    fixture.remove_machine("delayed");

    let aggregate = fixture.create_fake_machine("aggregate", "delayed-ready", None)?;
    require_success(&aggregate, "aggregate machine create")?;
    let mut aggregate_stream = open_http_request(
        &server.socket,
        "POST",
        "/v1/machines/aggregate/start",
        br#"{"wait":false,"timeout_s":20}"#,
        Some("application/json"),
    )?;
    aggregate_stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut first = [0_u8; 1];
    match aggregate_stream.read(&mut first) {
        Err(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        Ok(0) => return Err("JSON aggregation closed before the action completed".into()),
        Ok(_) => return Err("JSON aggregation sent bytes before the action completed".into()),
        Err(source) => return Err(source.into()),
    }
    aggregate_stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    let mut aggregate_response = Vec::new();
    aggregate_stream.read_to_end(&mut aggregate_response)?;
    let aggregate_response = parse_http_response(&aggregate_response)?;
    assert_eq!(aggregate_response.status, 200);
    assert_eq!(
        aggregate_response
            .headers
            .get("content-type")
            .map(String::as_str),
        Some("application/json")
    );
    let aggregate_json: Value = serde_json::from_slice(&aggregate_response.body)?;
    assert!(
        aggregate_json["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
    );
    assert_eq!(aggregate_json["result"]["type"], "Result");
    assert_eq!(aggregate_json["result"]["action"], "start");
    fixture.remove_machine("aggregate");

    let disconnect = fixture.create_fake_machine("disconnect", "delayed-ready", None)?;
    require_success(&disconnect, "disconnect machine create")?;
    let mut disconnected_stream = open_http_request(
        &server.socket,
        "POST",
        "/v1/machines/disconnect/start",
        br#"{"wait":false,"timeout_s":20}"#,
        None,
    )?;
    let _ = read_until(
        &mut disconnected_stream,
        b"\"type\":\"StepStart\"",
        Duration::from_millis(700),
    )?;
    drop(disconnected_stream);
    let state = wait_for_state(
        &fixture.home,
        "disconnect",
        "running",
        Duration::from_secs(8),
    )?;
    let visible_running = if cfg!(target_os = "linux") {
        "running"
    } else {
        "running (unsupervised)"
    };
    wait_for_list_status(
        &server.socket,
        "disconnect",
        visible_running,
        Duration::from_secs(8),
    )?;
    let shim_pid = state["shim_pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("running state has no shim pid")?;
    let vmm_pid = state["vmm_pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("running state has no VMM pid")?;
    assert!(kill(Pid::from_raw(shim_pid), None).is_ok());
    assert!(kill(Pid::from_raw(vmm_pid), None).is_ok());

    let killed = server.signal(Signal::SIGKILL)?;
    assert!(!killed.status.success());
    assert!(kill(Pid::from_raw(shim_pid), None).is_ok());
    assert!(kill(Pid::from_raw(vmm_pid), None).is_ok());

    let restarted = Server::spawn(&fixture)?;
    let machines = http_request(&restarted.socket, "GET", "/v1/machines", b"", None)?;
    assert_eq!(machines.status, 200);
    let machines: Value = serde_json::from_slice(&machines.body)?;
    let running = machines.as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["name"] == "disconnect" && row["status"] == visible_running)
    });
    assert!(running, "restarted serve returned machines {machines}");
    fixture.remove_machine("disconnect");

    let output = restarted.signal(Signal::SIGTERM)?;
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    Ok(())
}

/// Replaces one JSON scalar with a marker so volatile sample fields can be
/// byte-compared across two independent samples.
fn redact_scalar(payload: &[u8], field: &str) -> TestResult<Vec<u8>> {
    let marker = format!("\"{field}\":").into_bytes();
    let start = payload
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|index| index + marker.len())
        .ok_or_else(|| format!("metrics payload has no {field}"))?;
    let end = start
        + payload[start..]
            .iter()
            .position(|byte| *byte == b',' || *byte == b'}')
            .ok_or_else(|| format!("metrics payload {field} is unterminated"))?;
    let mut redacted = payload[..start].to_vec();
    redacted.extend_from_slice(format!("<{field}>").as_bytes());
    redacted.extend_from_slice(&payload[end..]);
    Ok(redacted)
}

fn redact_metrics_sample(payload: &[u8]) -> TestResult<Vec<u8>> {
    let mut redacted = payload.to_vec();
    for field in ["sampled_at", "cpu_time_ns", "rss_bytes"] {
        redacted = redact_scalar(&redacted, field)?;
    }
    Ok(redacted)
}

#[test]
fn real_cli_and_unix_rest_project_one_metrics_sample_and_conflict() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;
    require_success(
        &fixture.create_fake_machine("sampled", "normal", Some(2))?,
        "metrics machine create",
    )?;

    let stopped_cli = fixture.json_command(&["metrics", "sampled"])?;
    assert_eq!(stopped_cli.status.code(), Some(4));
    let stopped_rest = http_request(
        &server.socket,
        "GET",
        "/v1/machines/sampled/metrics",
        b"",
        None,
    )?;
    assert_eq!(stopped_rest.status, 409);
    assert_eq!(stopped_rest.body, error_bytes(&stopped_cli.stdout)?);

    require_success(
        &fixture.json_command(&["start", "sampled", "--no-wait", "--timeout", "20s"])?,
        "metrics machine start",
    )?;

    let metrics_cli = fixture.json_command(&["metrics", "sampled"])?;
    require_success(&metrics_cli, "CLI metrics")?;
    let metrics_cli = terminal_result(&metrics_cli.stdout, "metrics")?;
    let metrics_rest = http_request(
        &server.socket,
        "GET",
        "/v1/machines/sampled/metrics",
        b"",
        None,
    )?;
    assert_eq!(metrics_rest.status, 200);
    assert_eq!(
        metrics_rest.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        redact_metrics_sample(&metrics_rest.body)?,
        redact_metrics_sample(&metrics_cli.payload_bytes)?,
        "metrics stable payload bytes or field order changed"
    );

    let payload = &metrics_cli.payload;
    assert_eq!(payload["cpu"]["vcpus"], 2);
    assert_eq!(payload["memory"]["allocated_bytes"], 2_147_483_648_u64);
    assert_eq!(payload["memory"]["guest_actual_bytes"], 1);
    assert_eq!(payload["net"], Value::Null);
    assert_eq!(
        payload["block"],
        json!([
            {
                "device": "_disk0",
                "read_bytes": 4096,
                "written_bytes": 8192,
                "read_ops": 2,
                "write_ops": 3
            },
            {
                "device": "_disk1",
                "read_bytes": 0,
                "written_bytes": 0,
                "read_ops": 0,
                "write_ops": 0
            }
        ])
    );
    assert!(
        payload["sampled_at"]
            .as_str()
            .is_some_and(|value| value.ends_with('Z')),
        "sampled_at must be an RFC 3339 instant: {payload}"
    );

    let rest_text = String::from_utf8(metrics_rest.body.clone())?;
    assert!(
        !rest_text.contains("18446744073709551615"),
        "a u64::MAX sentinel reached the metrics payload: {rest_text}"
    );
    assert!(
        !rest_text.contains("latency"),
        "an unprojected latency counter reached the metrics payload: {rest_text}"
    );

    let mut human = fixture.command();
    human.args(["metrics", "sampled"]);
    let human = run_bounded(human, COMMAND_TIMEOUT)?;
    require_success(&human, "human metrics")?;
    let human_text = String::from_utf8(human.stdout)?;
    assert!(human_text.starts_with("sampled at "), "{human_text}");
    assert!(human_text.contains("cpu       2 vcpus"), "{human_text}");
    assert!(
        human_text.contains("net       none reported"),
        "{human_text}"
    );

    fixture.remove_machine("sampled");
    let output = server.signal(Signal::SIGTERM)?;
    assert!(output.status.success());
    Ok(())
}

#[test]
fn real_cli_lock_blocks_rest_patch_until_editor_releases() -> TestResult {
    let fixture = Fixture::new()?;
    let server = Server::spawn(&fixture)?;
    let created = fixture.create_fake_machine("locked", "normal", None)?;
    require_success(&created, "lock machine create")?;

    let ready = fixture.root.join("editor.ready");
    let release = fixture.root.join("editor.release");
    let editor = fixture.root.join("blocking-editor.sh");
    fs::write(
        &editor,
        b"#!/bin/sh\nset -eu\n: > \"$FIRESTONE_EDITOR_READY\"\nwhile [ ! -e \"$FIRESTONE_EDITOR_RELEASE\" ]; do sleep 0.02; done\n",
    )?;
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700))?;
    let mut edit = fixture.command();
    edit.args(["--json", "edit", "locked"])
        .env("VISUAL", format!("sh {}", editor.display()))
        .env("FIRESTONE_EDITOR_READY", &ready)
        .env("FIRESTONE_EDITOR_RELEASE", &release)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut edit = edit.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        if Instant::now() >= deadline {
            edit.kill()?;
            let _ = edit.wait();
            return Err("editor did not acquire the machine lock".into());
        }
        thread::sleep(Duration::from_millis(10));
    }

    let socket = server.socket.clone();
    let request = thread::spawn(move || {
        http_request(
            &socket,
            "PATCH",
            "/v1/machines/locked",
            br#"{"cpus":3}"#,
            None,
        )
    });
    thread::sleep(Duration::from_millis(200));
    assert!(
        !request.is_finished(),
        "REST patch bypassed the CLI machine lock"
    );
    fs::write(&release, b"")?;
    if edit.wait_timeout(Duration::from_secs(5))?.is_none() {
        edit.kill()?;
        let _ = edit.wait();
        return Err("editor did not exit after release".into());
    }
    let edit = edit.wait_with_output()?;
    require_success(&edit, "locked CLI edit")?;
    let response = request.join().map_err(|_| "REST patch thread panicked")??;
    assert_eq!(response.status, 200);
    let payload: Value = serde_json::from_slice(&response.body)?;
    assert_eq!(payload["spec"]["cpus"], 3);

    fixture.remove_machine("locked");
    let output = server.signal(Signal::SIGTERM)?;
    assert!(output.status.success());
    Ok(())
}
