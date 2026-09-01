//! SPEC section 24 clone coverage over the fake VMM and fake qemu-img harness.

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
use serde_json::Value;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    path: OsString,
    fake_vmm: PathBuf,
    qemu_log: PathBuf,
    source: PathBuf,
    firmware: PathBuf,
    names: Vec<String>,
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
        let fake_vmm = compile_fake_vmm(&root)?;
        let bin = root.join("bin");
        fs::create_dir(&bin)?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
        let qemu_img = bin.join("qemu-img");
        fs::copy(&fake_vmm, &qemu_img)?;
        fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))?;
        let qemu_log = qemu_img.with_extension("qemu.log");
        let mut path_entries = vec![bin];
        if let Some(existing) = env::var_os("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        let path = env::join_paths(path_entries)?;

        let source = root.join("base.qcow2");
        fs::write(&source, b"QFI\xfbM6-CLONE")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        let firmware = root.join("firmware.fd");
        fs::write(&firmware, b"firmware")?;
        fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            _directory: directory,
            root,
            home,
            path,
            fake_vmm,
            qemu_log,
            source,
            firmware,
            names: Vec::new(),
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
        command
            .arg("--home")
            .arg(&self.home)
            .env("PATH", &self.path)
            .stdin(Stdio::null());
        command
    }

    fn json(&self, arguments: &[&str]) -> TestResult<Output> {
        let mut command = self.command();
        command.arg("--json").args(arguments);
        Ok(command.output()?)
    }

    fn create(&mut self, name: &str) -> TestResult {
        self.names.push(name.to_owned());
        let console = self.root.join(format!("{name}-console.log"));
        let mut command = self.command();
        command
            .arg("--json")
            .arg("create")
            .arg(name)
            .arg(&self.source)
            .arg("--net")
            .arg("none")
            .arg("--no-provisioning")
            .arg("--vmm-binary")
            .arg(&self.fake_vmm)
            .arg("--vmm-firmware")
            .arg(&self.firmware);
        for value in [
            "--record".to_owned(),
            self.root
                .join(format!("{name}-requests.log"))
                .to_string_lossy()
                .into_owned(),
            "--body".to_owned(),
            self.root
                .join(format!("{name}-body.json"))
                .to_string_lossy()
                .into_owned(),
            "--behavior".to_owned(),
            "normal".to_owned(),
            "--console-log".to_owned(),
            console.to_string_lossy().into_owned(),
        ] {
            command.arg(format!("--vmm-arg={value}"));
        }
        let output = command.output()?;
        require_success(&output, &format!("create {name}"))
    }

    fn machine_dir(&self, name: &str) -> PathBuf {
        self.home.join("data/machines").join(name)
    }

    fn state(&self, name: &str) -> TestResult<Value> {
        Ok(serde_json::from_slice(&fs::read(
            self.machine_dir(name).join("state.json"),
        )?)?)
    }

    fn qemu_log_len(&self) -> TestResult<usize> {
        Ok(fs::read_to_string(&self.qemu_log)
            .unwrap_or_default()
            .lines()
            .count())
    }

    fn qemu_log_since(&self, offset: usize) -> TestResult<Vec<String>> {
        Ok(fs::read_to_string(&self.qemu_log)
            .unwrap_or_default()
            .lines()
            .skip(offset)
            .map(str::to_owned)
            .collect())
    }

    fn track(&mut self, name: &str) {
        self.names.push(name.to_owned());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in &self.names {
            let mut stop = self.command();
            let _ = stop
                .args(["stop", name, "--force", "--timeout", "1s"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let mut remove = self.command();
            let _ = remove
                .args(["rm", name, "--force"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
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

fn require_success(output: &Output, label: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
}

fn ndjson(output: &Output) -> TestResult<Vec<Value>> {
    Ok(String::from_utf8(output.stdout.clone())?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()?)
}

fn terminal_result(output: &Output, action: &str) -> TestResult<Value> {
    let events = ndjson(output)?;
    let last = events.last().ok_or("action produced no events")?;
    if last["type"] != "Result" || last["action"] != action {
        return Err(format!("last event is not the {action} result: {last}").into());
    }
    Ok(last["payload"].clone())
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut server = Self {
            child: Some(command.spawn()?),
            socket: fixture.home.join("run/serve.sock"),
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
            if Instant::now() >= deadline {
                return Err("serve did not publish its socket before the deadline".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
        thread::sleep(Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn http_post_json(socket: &Path, path: &str, body: &[u8]) -> TestResult<HttpResponse> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;

    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("HTTP response has no header terminator")?;
    let header_text = std::str::from_utf8(&response[..header_end])?;
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
    let body = match headers.get("content-length") {
        Some(length) => {
            let length = length.parse::<usize>()?;
            wire_body
                .get(..length)
                .ok_or("HTTP response body is shorter than Content-Length")?
                .to_vec()
        }
        None => wire_body.to_vec(),
    };
    Ok(HttpResponse { status, body })
}

#[test]
fn clone_stopped_machine_copies_spec_bytes_overlay_and_boots() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("src")?;
    let started = fixture.json(&["start", "src", "--no-wait", "--timeout", "20s"])?;
    require_success(&started, "start src")?;
    let stopped = fixture.json(&["stop", "src", "--timeout", "10s"])?;
    require_success(&stopped, "stop src")?;

    let source_state = fixture.state("src")?;
    let source_mac = source_state["mac"]
        .as_str()
        .ok_or("started source has no MAC")?
        .to_owned();
    let source_instance_id = source_state["instance_id"]
        .as_str()
        .ok_or("started source has no instance id")?
        .to_owned();

    let offset = fixture.qemu_log_len()?;
    fixture.track("copy");
    let cloned = fixture.json(&["clone", "src", "copy"])?;
    require_success(&cloned, "clone src copy")?;
    let payload = terminal_result(&cloned, "clone")?;
    assert_eq!(payload["source"], "src");
    assert_eq!(payload["dest"], "copy");
    assert!(
        payload["disk_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );

    let convert = fixture
        .qemu_log_since(offset)?
        .into_iter()
        .find(|line| line.starts_with("convert "))
        .ok_or("clone did not run qemu-img convert")?;
    assert!(convert.contains(" -B "), "{convert}");
    assert!(convert.contains("backing_fmt=qcow2"), "{convert}");

    // The destination spec is the source document byte for byte.
    assert_eq!(
        fs::read(fixture.machine_dir("src").join("firestone.toml"))?,
        fs::read(fixture.machine_dir("copy").join("firestone.toml"))?
    );

    // Runtime identity is never copied.
    let clone_state = fixture.state("copy")?;
    assert_eq!(clone_state["status"], "created");
    assert_eq!(clone_state["mac"], Value::Null);
    assert_eq!(clone_state["instance_id"], Value::Null);
    assert_eq!(clone_state["shim_pid"], Value::Null);
    assert_eq!(clone_state["vmm_pid"], Value::Null);
    assert_eq!(clone_state["started_at"], Value::Null);
    assert_eq!(clone_state["last_exit"], Value::Null);
    assert_eq!(clone_state["image"], source_state["image"]);

    let clone_dir = fixture.machine_dir("copy");
    assert!(clone_dir.join("disk.qcow2").is_file());
    assert!(!clone_dir.join("disk.qcow2.partial").exists());
    for leaked in [
        "known_hosts",
        "seed.img",
        "seed",
        "vmconfig.json",
        "console.log",
        "snapshots",
        ".creating",
    ] {
        assert!(
            !clone_dir.join(leaked).exists(),
            "clone kept source artifact {leaked}"
        );
    }

    // The clone boots, and derives its own MAC and instance id from its name.
    let clone_started = fixture.json(&["start", "copy", "--no-wait", "--timeout", "20s"])?;
    require_success(&clone_started, "start copy")?;
    let booted = fixture.state("copy")?;
    assert_eq!(booted["status"], "running");
    assert_ne!(booted["mac"].as_str(), Some(source_mac.as_str()));
    assert_ne!(
        booted["instance_id"].as_str(),
        Some(source_instance_id.as_str())
    );
    assert!(
        booted["instance_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("iid-copy-")),
        "{booted}"
    );
    Ok(())
}

#[test]
fn clone_running_source_is_refused_with_a_stop_hint() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("busy")?;
    let started = fixture.json(&["start", "busy", "--no-wait", "--timeout", "20s"])?;
    require_success(&started, "start busy")?;

    let refused = fixture.json(&["clone", "busy", "busy-copy"])?;
    assert!(!refused.status.success());
    let events = ndjson(&refused)?;
    let error = events.last().ok_or("refusal produced no events")?;
    assert_eq!(error["error"]["kind"], "conflict");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("running")),
        "{error}"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("stop busy")),
        "{error}"
    );
    assert!(!fixture.machine_dir("busy-copy").exists());

    let self_clone = fixture.json(&["clone", "busy", "busy"])?;
    assert!(!self_clone.status.success());
    let events = ndjson(&self_clone)?;
    let error = events.last().ok_or("self clone produced no events")?;
    assert_eq!(error["error"]["kind"], "usage");
    Ok(())
}

#[test]
fn clone_fresh_disk_creates_an_empty_overlay_on_the_same_base() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("origin")?;
    let started = fixture.json(&["start", "origin", "--no-wait", "--timeout", "20s"])?;
    require_success(&started, "start origin")?;
    let stopped = fixture.json(&["stop", "origin", "--timeout", "10s"])?;
    require_success(&stopped, "stop origin")?;

    let offset = fixture.qemu_log_len()?;
    fixture.track("fresh");
    let cloned = fixture.json(&["clone", "origin", "fresh", "--fresh-disk"])?;
    require_success(&cloned, "clone --fresh-disk")?;
    let payload = terminal_result(&cloned, "clone")?;
    assert_eq!(payload["dest"], "fresh");

    let commands = fixture.qemu_log_since(offset)?;
    assert!(
        commands.iter().any(|line| line.starts_with("create ")),
        "{commands:?}"
    );
    assert!(
        !commands.iter().any(|line| line.starts_with("convert ")),
        "{commands:?}"
    );
    assert!(fixture.machine_dir("fresh").join("disk.qcow2").is_file());
    assert_eq!(
        fs::read(fixture.machine_dir("origin").join("firestone.toml"))?,
        fs::read(fixture.machine_dir("fresh").join("firestone.toml"))?
    );
    Ok(())
}

#[test]
fn clone_over_rest_projects_the_same_result_payload_as_the_cli() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("shared")?;
    let started = fixture.json(&["start", "shared", "--no-wait", "--timeout", "20s"])?;
    require_success(&started, "start shared")?;
    let stopped = fixture.json(&["stop", "shared", "--timeout", "10s"])?;
    require_success(&stopped, "stop shared")?;

    fixture.track("cli-copy");
    let cli = fixture.json(&["clone", "shared", "cli-copy"])?;
    require_success(&cli, "clone shared cli-copy")?;
    let cli_payload = terminal_result(&cli, "clone")?;

    let server = Server::spawn(&fixture)?;
    fixture.track("rest-copy");
    let response = http_post_json(
        &server.socket,
        "/v1/machines/shared/clone",
        br#"{"name":"rest-copy"}"#,
    )?;
    assert_eq!(response.status, 200);
    let aggregated: Value = serde_json::from_slice(&response.body)?;
    assert_eq!(aggregated["result"]["action"], "clone");
    let rest_payload = aggregated["result"]["payload"].clone();

    assert_eq!(rest_payload["source"], cli_payload["source"]);
    assert_eq!(rest_payload["dest"], "rest-copy");
    assert_eq!(rest_payload["disk_bytes"], cli_payload["disk_bytes"]);
    assert_eq!(
        rest_payload
            .as_object()
            .map(|payload| payload.keys().map(String::as_str).collect::<Vec<_>>()),
        cli_payload
            .as_object()
            .map(|payload| payload.keys().map(String::as_str).collect::<Vec<_>>())
    );

    let source_spec = fs::read(fixture.machine_dir("shared").join("firestone.toml"))?;
    assert_eq!(
        fs::read(fixture.machine_dir("cli-copy").join("firestone.toml"))?,
        source_spec
    );
    assert_eq!(
        fs::read(fixture.machine_dir("rest-copy").join("firestone.toml"))?,
        source_spec
    );

    let taken = http_post_json(
        &server.socket,
        "/v1/machines/shared/clone",
        br#"{"name":"cli-copy"}"#,
    )?;
    let envelope: Value = serde_json::from_slice(&taken.body)?;
    assert_eq!(envelope["error"]["kind"], "already_exists");
    Ok(())
}
