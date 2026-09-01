//! SPEC section 23 snapshot coverage over the fake VMM and fake qemu-img harness.

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
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    path: OsString,
    fake_vmm: PathBuf,
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
        let mut path_entries = vec![bin];
        if let Some(existing) = env::var_os("PATH") {
            path_entries.extend(env::split_paths(&existing));
        }
        let path = env::join_paths(path_entries)?;

        let source = root.join("base.qcow2");
        fs::write(&source, b"QFI\xfbM6-SNAPSHOT")?;
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
            self.requests(name).to_string_lossy().into_owned(),
            "--body".to_owned(),
            self.root
                .join(format!("{name}-body.json"))
                .to_string_lossy()
                .into_owned(),
            "--behavior".to_owned(),
            "normal".to_owned(),
            "--console-log".to_owned(),
            self.console(name).to_string_lossy().into_owned(),
        ] {
            command.arg(format!("--vmm-arg={value}"));
        }
        let output = command.output()?;
        require_success(&output, &format!("create {name}"))
    }

    fn machine_dir(&self, name: &str) -> PathBuf {
        self.home.join("data/machines").join(name)
    }

    fn snapshot_dir(&self, name: &str, snapshot: &str) -> PathBuf {
        self.machine_dir(name).join("snapshots").join(snapshot)
    }

    fn requests(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}-requests.log"))
    }

    fn console(&self, name: &str) -> PathBuf {
        self.machine_dir(name).join("console.log")
    }

    fn recorded_requests(&self, name: &str) -> TestResult<Vec<String>> {
        Ok(fs::read_to_string(self.requests(name))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("GET ") || line.starts_with("PUT "))
            .map(str::to_owned)
            .collect())
    }

    fn start(&self, name: &str) -> TestResult {
        let output = self.json(&["start", name, "--no-wait", "--timeout", "20s"])?;
        require_success(&output, &format!("start {name}"))
    }

    fn stop(&self, name: &str) -> TestResult {
        let output = self.json(&["stop", name, "--timeout", "10s"])?;
        require_success(&output, &format!("stop {name}"))
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

fn terminal_error(output: &Output) -> TestResult<Value> {
    if output.status.success() {
        return Err(format!(
            "expected a refusal, got stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
        .into());
    }
    let events = ndjson(output)?;
    let last = events.last().ok_or("refusal produced no events")?;
    Ok(last["error"].clone())
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

fn http_request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> TestResult<HttpResponse> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    match body {
        Some(body) => {
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )?;
            stream.write_all(body)?;
        }
        None => write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: firestone\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        )?,
    }
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
fn snapshot_cold_round_trip_restores_the_disk_spec_and_vmconfig() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("cold")?;
    fixture.start("cold")?;
    fixture.stop("cold")?;

    let spec_before = fs::read(fixture.machine_dir("cold").join("firestone.toml"))?;
    let disk_before = fs::read(fixture.machine_dir("cold").join("disk.qcow2"))?;
    let vmconfig_before = fs::read(fixture.machine_dir("cold").join("vmconfig.json"))?;

    let created = fixture.json(&["snapshot", "create", "cold", "before-upgrade"])?;
    require_success(&created, "snapshot create")?;
    let payload = terminal_result(&created, "snapshot-create")?;
    assert_eq!(payload["name"], "cold");
    assert_eq!(payload["snapshot"], "before-upgrade");
    assert_eq!(payload["kind"], "cold");
    assert_eq!(payload["memory_bytes"], Value::Null);
    assert!(payload["disk_bytes"].as_u64().is_some_and(|size| size > 0));

    // The snapshot directory carries exactly the documented members.
    let directory = fixture.snapshot_dir("cold", "before-upgrade");
    assert_eq!(
        fs::symlink_metadata(&directory)?.mode() & 0o7777,
        0o700,
        "snapshot directory must be owner-only"
    );
    for member in ["metadata.json", "spec.toml", "disk.qcow2", "vmconfig.json"] {
        assert!(directory.join(member).is_file(), "missing {member}");
    }
    assert!(!directory.join("vmstate").exists());
    assert_eq!(fs::read(directory.join("spec.toml"))?, spec_before);
    assert_eq!(fs::read(directory.join("vmconfig.json"))?, vmconfig_before);

    let metadata: Value = serde_json::from_slice(&fs::read(directory.join("metadata.json"))?)?;
    assert_eq!(metadata["schema_version"], 1);
    assert_eq!(metadata["kind"], "cold");
    assert!(metadata["image_id"].as_str().is_some());
    assert!(metadata["memory_bytes"].is_null());

    // The snapshot is immutable: later machine changes do not reach it.
    fs::write(
        fixture.machine_dir("cold").join("firestone.toml"),
        b"image = \"changed\"\n",
    )?;
    fs::write(
        fixture.machine_dir("cold").join("disk.qcow2"),
        b"QFI\xfbJUNK",
    )?;

    let listed = fixture.json(&["snapshot", "list", "cold"])?;
    require_success(&listed, "snapshot list")?;
    let rows = terminal_result(&listed, "snapshot-list")?;
    assert_eq!(rows["snapshots"].as_array().map(Vec::len), Some(1));
    assert_eq!(rows["snapshots"][0]["snapshot"], "before-upgrade");
    assert_eq!(rows["snapshots"][0]["kind"], "cold");

    let restored = fixture.json(&["snapshot", "restore", "cold", "before-upgrade"])?;
    require_success(&restored, "snapshot restore")?;
    let payload = terminal_result(&restored, "snapshot-restore")?;
    assert_eq!(payload["snapshot"], "before-upgrade");
    assert_eq!(payload["started"], false);

    assert_eq!(
        fs::read(fixture.machine_dir("cold").join("firestone.toml"))?,
        spec_before
    );
    assert_eq!(
        fs::read(fixture.machine_dir("cold").join("disk.qcow2"))?,
        disk_before
    );
    assert_eq!(
        fs::read(fixture.machine_dir("cold").join("vmconfig.json"))?,
        vmconfig_before
    );
    // A cold restore never writes the warm launch marker.
    assert!(
        !fixture
            .machine_dir("cold")
            .join("restore-request.json")
            .exists()
    );
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.machine_dir("cold").join("state.json"))?)?;
    assert_eq!(state["status"], "stopped");

    // A snapshot pins its base image against the image store.
    let images: Value =
        serde_json::from_slice(&fs::read(fixture.machine_dir("cold").join("state.json"))?)?;
    let image_id = images["image"]["id"]
        .as_str()
        .ok_or("machine has no pinned image")?
        .to_owned();
    let removed = fixture.json(&["images", "rm", &image_id])?;
    let error = terminal_error(&removed)?;
    assert_eq!(error["kind"], "conflict");

    // rm warns about the snapshots it is about to delete with the machine.
    let removal = fixture.json(&["rm", "cold"])?;
    require_success(&removal, "rm cold")?;
    let warned = ndjson(&removal)?.into_iter().any(|event| {
        event["type"] == "Log"
            && event["level"] == "warn"
            && event["message"]
                .as_str()
                .is_some_and(|message| message.contains("before-upgrade"))
    });
    assert!(warned, "rm did not warn about the machine's snapshots");
    assert!(!fixture.machine_dir("cold").exists());
    Ok(())
}

#[test]
fn snapshot_warm_pauses_captures_and_resumes_in_that_order() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("warm")?;
    fixture.start("warm")?;

    let created = fixture.json(&["snapshot", "create", "warm", "live"])?;
    require_success(&created, "warm snapshot create")?;
    let payload = terminal_result(&created, "snapshot-create")?;
    assert_eq!(payload["kind"], "warm");
    assert!(
        payload["memory_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );

    let requests = fixture.recorded_requests("warm")?;
    let pause = requests
        .iter()
        .position(|line| line == "PUT /api/v1/vm.pause")
        .ok_or("warm snapshot did not pause the VM")?;
    let capture = requests
        .iter()
        .position(|line| line == "PUT /api/v1/vm.snapshot")
        .ok_or("warm snapshot did not call vm.snapshot")?;
    let resume = requests
        .iter()
        .position(|line| line == "PUT /api/v1/vm.resume")
        .ok_or("warm snapshot did not resume the VM")?;
    assert!(pause < capture && capture < resume, "{requests:?}");

    let directory = fixture.snapshot_dir("warm", "live");
    for member in ["config.json", "state.json", "memory-ranges"] {
        assert!(
            directory.join("vmstate").join(member).is_file(),
            "missing vmstate/{member}"
        );
    }
    // The captured memory image keeps its holes: the copy Firestone made of the
    // overlay and the state Cloud Hypervisor wrote are both sparse.
    let ranges = fs::symlink_metadata(directory.join("vmstate").join("memory-ranges"))?;
    assert!(ranges.len() > 0);
    assert!(
        ranges.blocks().saturating_mul(512) < ranges.len(),
        "memory-ranges was materialized: {} bytes in {} blocks",
        ranges.len(),
        ranges.blocks()
    );

    let metadata: Value = serde_json::from_slice(&fs::read(directory.join("metadata.json"))?)?;
    assert_eq!(metadata["kind"], "warm");
    assert!(metadata["memory_bytes"].as_u64().is_some_and(|b| b > 0));

    // The machine is still running afterwards, and its state is not degraded.
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.machine_dir("warm").join("state.json"))?)?;
    assert_eq!(state["status"], "running");
    assert_eq!(state["degraded"].as_array().map(Vec::len), Some(0));
    Ok(())
}

#[test]
fn snapshot_warm_restore_relaunches_through_the_shim_restore_mode() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("resume")?;
    fixture.start("resume")?;
    let created = fixture.json(&["snapshot", "create", "resume", "live"])?;
    require_success(&created, "warm snapshot create")?;

    let restored = fixture.json(&["snapshot", "restore", "resume", "live", "--force"])?;
    require_success(&restored, "warm snapshot restore")?;
    let payload = terminal_result(&restored, "snapshot-restore")?;
    assert_eq!(payload["started"], true);

    // The relaunch restored and resumed instead of creating and booting a VM.
    let requests = fixture.recorded_requests("resume")?;
    assert!(
        requests.iter().any(|line| line == "PUT /api/v1/vm.restore"),
        "{requests:?}"
    );
    assert!(
        requests.iter().any(|line| line == "PUT /api/v1/vm.resume"),
        "{requests:?}"
    );
    assert!(
        !requests.iter().any(|line| line == "PUT /api/v1/vm.create"),
        "{requests:?}"
    );
    assert!(
        !requests.iter().any(|line| line == "PUT /api/v1/vm.boot"),
        "{requests:?}"
    );

    // The marker is consumed, and the console history survived the truncation
    // Cloud Hypervisor performs on restore.
    assert!(
        !fixture
            .machine_dir("resume")
            .join("restore-request.json")
            .exists()
    );
    assert!(
        fixture
            .machine_dir("resume")
            .join("console.log.previous")
            .is_file()
    );
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.machine_dir("resume").join("state.json"))?)?;
    assert_eq!(state["status"], "running");
    Ok(())
}

#[test]
fn snapshot_restore_vmconfig_mismatch_refuses_the_launch() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("drift")?;
    fixture.start("drift")?;
    let created = fixture.json(&["snapshot", "create", "drift", "live"])?;
    require_success(&created, "warm snapshot create")?;
    fixture.stop("drift")?;

    // Rewrite the snapshot's own spec so the restored machine can no longer
    // publish the VmConfig the snapshot captured.
    let spec_path = fixture.snapshot_dir("drift", "live").join("spec.toml");
    let spec = fs::read_to_string(&spec_path)?;
    let drifted = spec
        .lines()
        .map(|line| {
            if line.starts_with("cpus = ") {
                "cpus = 3".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(drifted.contains("cpus = 3"), "spec has no cpus line");
    fs::write(&spec_path, format!("{drifted}\n"))?;

    let refused = fixture.json(&["snapshot", "restore", "drift", "live"])?;
    let error = terminal_error(&refused)?;
    assert_eq!(error["kind"], "conflict");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("VmConfig")),
        "{error}"
    );
    assert!(error["hint"].as_str().is_some(), "{error}");
    Ok(())
}

#[test]
fn snapshot_refusals_cover_running_missing_and_partial_directories() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("guard")?;
    fixture.start("guard")?;
    let created = fixture.json(&["snapshot", "create", "guard", "live"])?;
    require_success(&created, "warm snapshot create")?;

    // A running machine is refused without --force.
    let refused = fixture.json(&["snapshot", "restore", "guard", "live"])?;
    let error = terminal_error(&refused)?;
    assert_eq!(error["kind"], "conflict");
    assert!(
        error["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("--force")),
        "{error}"
    );

    // A snapshot that does not exist is a not_found, not a panic.
    let missing = fixture.json(&["snapshot", "restore", "guard", "absent"])?;
    assert_eq!(terminal_error(&missing)?["kind"], "not_found");
    let removed = fixture.json(&["snapshot", "rm", "guard", "absent"])?;
    assert_eq!(terminal_error(&removed)?["kind"], "not_found");

    // A name that is not a snapshot identifier never reaches the filesystem.
    let traversal = fixture.json(&["snapshot", "rm", "guard", "../escape"])?;
    assert_eq!(terminal_error(&traversal)?["kind"], "invalid_spec");

    // A half-written snapshot is invisible to list and to rm.
    let partial = fixture
        .machine_dir("guard")
        .join("snapshots")
        .join(".partial-half");
    fs::create_dir(&partial)?;
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o700))?;
    let listed = fixture.json(&["snapshot", "list", "guard"])?;
    require_success(&listed, "snapshot list")?;
    let rows = terminal_result(&listed, "snapshot-list")?;
    let names = rows["snapshots"]
        .as_array()
        .ok_or("snapshot list is not an array")?
        .iter()
        .map(|row| row["snapshot"].clone())
        .collect::<Vec<_>>();
    assert_eq!(names, vec![Value::from("live")]);
    let half = fixture.json(&["snapshot", "rm", "guard", "half"])?;
    assert_eq!(terminal_error(&half)?["kind"], "not_found");

    // Taking the same name twice is refused before anything is copied.
    let duplicate = fixture.json(&["snapshot", "create", "guard", "live"])?;
    assert_eq!(terminal_error(&duplicate)?["kind"], "already_exists");
    Ok(())
}

#[test]
fn snapshot_rest_routes_project_the_same_results_as_the_cli() -> TestResult {
    let mut fixture = Fixture::new()?;
    fixture.create("rest")?;
    fixture.start("rest")?;
    fixture.stop("rest")?;

    let cli = fixture.json(&["snapshot", "create", "rest", "cli-made"])?;
    require_success(&cli, "snapshot create")?;
    let cli_payload = terminal_result(&cli, "snapshot-create")?;

    let server = Server::spawn(&fixture)?;
    let created = http_request(
        &server.socket,
        "POST",
        "/v1/machines/rest/snapshots",
        Some(br#"{"snapshot":"rest-made"}"#),
    )?;
    assert_eq!(created.status, 200);
    let aggregated: Value = serde_json::from_slice(&created.body)?;
    assert_eq!(aggregated["result"]["action"], "snapshot-create");
    let rest_payload = aggregated["result"]["payload"].clone();
    assert_eq!(rest_payload["kind"], cli_payload["kind"]);
    assert_eq!(rest_payload["disk_bytes"], cli_payload["disk_bytes"]);
    assert_eq!(rest_payload["snapshot"], "rest-made");
    assert_eq!(
        rest_payload
            .as_object()
            .map(|payload| payload.keys().map(String::as_str).collect::<Vec<_>>()),
        cli_payload
            .as_object()
            .map(|payload| payload.keys().map(String::as_str).collect::<Vec<_>>())
    );

    let listed = http_request(&server.socket, "GET", "/v1/machines/rest/snapshots", None)?;
    assert_eq!(listed.status, 200);
    let rows: Value = serde_json::from_slice(&listed.body)?;
    let names = rows["snapshots"]
        .as_array()
        .ok_or("snapshot list is not an array")?
        .iter()
        .map(|row| row["snapshot"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![Value::from("cli-made"), Value::from("rest-made")]
    );

    let cli_listed = fixture.json(&["snapshot", "list", "rest"])?;
    require_success(&cli_listed, "snapshot list")?;
    assert_eq!(terminal_result(&cli_listed, "snapshot-list")?, rows);

    let restored = http_request(
        &server.socket,
        "POST",
        "/v1/machines/rest/snapshots/rest-made/restore",
        Some(br#"{"force":false,"start":false}"#),
    )?;
    assert_eq!(restored.status, 200);
    let aggregated: Value = serde_json::from_slice(&restored.body)?;
    assert_eq!(aggregated["result"]["action"], "snapshot-restore");
    assert_eq!(aggregated["result"]["payload"]["started"], false);

    let deleted = http_request(
        &server.socket,
        "DELETE",
        "/v1/machines/rest/snapshots/rest-made",
        None,
    )?;
    assert_eq!(deleted.status, 204);
    assert!(deleted.body.is_empty());
    assert!(!fixture.snapshot_dir("rest", "rest-made").exists());

    let gone = http_request(
        &server.socket,
        "DELETE",
        "/v1/machines/rest/snapshots/rest-made",
        None,
    )?;
    assert_eq!(gone.status, 404);
    let envelope: Value = serde_json::from_slice(&gone.body)?;
    assert_eq!(envelope["error"]["kind"], "not_found");
    Ok(())
}
