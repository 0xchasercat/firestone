use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn invalid_argument_json_requested_emits_structured_usage_error() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--json", "--bogus"])
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["kind"], "usage");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| { message.contains("unexpected argument '--bogus'") })
    );
    Ok(())
}

#[test]
fn doctor_failed_checks_emit_report_and_dependency_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args([
            "--json",
            "--home",
            home.to_str().ok_or("temporary home is not UTF-8")?,
            "doctor",
        ])
        .output()?;

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let records = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "Result");
    assert_eq!(records[0]["action"], "doctor");
    assert_eq!(
        records[0]["payload"]["checks"].as_array().map(Vec::len),
        Some(13)
    );
    Ok(())
}

#[test]
fn create_edit_editor_writes_stdout_emits_one_result_and_publishes_atomically() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let editor = root.join("editor.sh");
    fs::write(
        &editor,
        b"#!/bin/sh\n[ \"$1\" = \"--wait\" ] || exit 9\nprintf 'editor-noise\\n'\n",
    )?;
    let mut permissions = fs::metadata(&editor)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&editor, permissions)?;

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args([
            "--json",
            "--home",
            home.to_str().ok_or("temporary home is not UTF-8")?,
            "create",
            "demo",
            "ubuntu:24.04",
            "--edit",
        ])
        .env("VISUAL", format!("sh {} --wait", editor.display()))
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("editor-noise"));
    let stdout = String::from_utf8(output.stdout)?;
    let records = stdout
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        records
            .iter()
            .filter(|record| { record["type"] == "Result" && record["action"] == "create" })
            .count(),
        1
    );

    let machine_dir = home.join("data/machines/demo");
    assert!(machine_dir.join("firestone.toml").is_file());
    assert!(machine_dir.join("state.json").is_file());
    assert!(!machine_dir.join(".creating").exists());
    Ok(())
}

fn firestone(home: &Path, path: &OsString) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.arg("--home").arg(home).env("PATH", path);
    command
}

fn ndjson(output: &Output) -> Result<Vec<serde_json::Value>, Box<dyn Error>> {
    Ok(String::from_utf8(output.stdout.clone())?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()?)
}

fn compile_fake_vmm(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
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

fn create_fake_machine(
    home: &Path,
    path: &OsString,
    name: &str,
    source: &Path,
    firmware: &Path,
    fake_vmm: &Path,
    behavior: &str,
) -> Result<Output, Box<dyn Error>> {
    let root = source.parent().ok_or("image source has no parent")?;
    let machine_dir = home.join("data/machines").join(name);
    let record = root.join(format!("{name}-requests.log"));
    let body = root.join(format!("{name}-body.json"));
    let console = machine_dir.join("console.log");
    let mut command = firestone(home, path);
    command
        .arg("--json")
        .arg("create")
        .arg(name)
        .arg(source)
        .arg("--net")
        .arg("none")
        .arg("--no-provisioning")
        .arg("--vmm-binary")
        .arg(fake_vmm)
        .arg("--vmm-firmware")
        .arg(firmware);
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
    Ok(command.output()?)
}

struct MachineCleanup {
    home: PathBuf,
    path: OsString,
    names: Vec<String>,
}

impl Drop for MachineCleanup {
    fn drop(&mut self) {
        for name in &self.names {
            let _ = firestone(&self.home, &self.path)
                .args(["stop", name, "--force", "--timeout", "1s"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = firestone(&self.home, &self.path)
                .args(["rm", name, "--force"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

#[test]
fn lifecycle_cli_smoke_without_kvm() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let fake_vmm = compile_fake_vmm(&root)?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    let qemu_img = bin.join("qemu-img");
    fs::copy(&fake_vmm, &qemu_img)?;
    fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))?;
    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;
    let source = root.join("base.qcow2");
    fs::write(&source, b"QFI\xfbCLI-SMOKE")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let second_source = root.join("second.qcow2");
    fs::write(&second_source, b"QFI\xfbCLI-SMOKE-SECOND")?;
    fs::set_permissions(&second_source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        names: vec!["demo".to_owned(), "slow".to_owned()],
    };

    let created = create_fake_machine(
        &home, &path, "demo", &source, &firmware, &fake_vmm, "normal",
    )?;
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert_eq!(
        ndjson(&created)?
            .iter()
            .filter(|event| event["type"] == "Result" && event["action"] == "create")
            .count(),
        1
    );

    let start = firestone(&home, &path)
        .args(["--json", "start", "demo", "--no-wait", "--timeout", "20s"])
        .output()?;
    assert!(
        start.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(start.stderr.is_empty());
    let start_events = ndjson(&start)?;
    let mut position = 0_usize;
    for id in ["image", "disk", "seed", "shim", "net", "fs", "vmm"] {
        let relative = start_events[position..]
            .iter()
            .position(|event| event.get("id").and_then(serde_json::Value::as_str) == Some(id))
            .ok_or_else(|| format!("missing ordered start step {id}"))?;
        position += relative + 1;
    }
    assert!(!start_events.iter().any(|event| {
        matches!(
            event.get("id").and_then(serde_json::Value::as_str),
            Some("boot" | "ssh")
        )
    }));
    let start_result = start_events.last().ok_or("missing start result")?;
    assert_eq!(start_result["type"], "Result");
    assert_eq!(start_result["action"], "start");
    assert_eq!(start_result["payload"]["status"], "running");

    let duplicate = firestone(&home, &path)
        .args(["--json", "start", "demo", "--timeout", "2s"])
        .output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    assert_eq!(ndjson(&duplicate)?[0]["error"]["kind"], "already_running");

    let vmconfig_path = home.join("data/machines/demo/vmconfig.json");
    let mut expected_vmconfig = fs::read(&vmconfig_path)?;
    expected_vmconfig.push(b'\n');
    let vmconfig = firestone(&home, &path)
        .args(["show", "demo", "--vmconfig"])
        .output()?;
    assert!(vmconfig.status.success());
    assert_eq!(vmconfig.stdout, expected_vmconfig);

    let logs = firestone(&home, &path)
        .args(["logs", "demo", "-n", "1"])
        .output()?;
    assert!(logs.status.success());
    assert_eq!(logs.stdout, b"current boot\n");

    let follower = firestone(&home, &path)
        .args(["logs", "demo", "-f", "-n", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    thread::sleep(Duration::from_millis(300));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(follower.id())?),
        nix::sys::signal::Signal::SIGINT,
    )?;
    let followed = follower.wait_with_output()?;
    assert_eq!(followed.status.code(), Some(130));
    assert!(String::from_utf8_lossy(&followed.stderr).contains("interrupted"));

    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(home.join("data/machines/demo/state.json"))?)?;
    let image_id = state["image"]["id"]
        .as_str()
        .ok_or("state has no image id")?
        .to_owned();
    let list = firestone(&home, &path)
        .args(["--json", "images", "ls"])
        .output()?;
    assert!(list.status.success());
    assert_eq!(
        ndjson(&list)?[0]["payload"].as_array().map(Vec::len),
        Some(1)
    );
    let inspect = firestone(&home, &path)
        .args(["--json", "images", "inspect", &image_id])
        .output()?;
    assert!(inspect.status.success());
    assert_eq!(
        ndjson(&inspect)?[0]["payload"]["image"]["metadata"]["id"],
        image_id
    );
    let referenced_remove = firestone(&home, &path)
        .args(["--json", "images", "rm", &image_id])
        .output()?;
    assert_eq!(referenced_remove.status.code(), Some(4));
    assert_eq!(ndjson(&referenced_remove)?[0]["error"]["kind"], "conflict");

    let restart = firestone(&home, &path)
        .args(["--json", "restart", "demo"])
        .output()?;
    assert!(
        restart.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&restart.stdout),
        String::from_utf8_lossy(&restart.stderr)
    );
    let restart_events = ndjson(&restart)?;
    assert_eq!(
        restart_events.last().ok_or("missing restart result")?["action"],
        "restart"
    );
    assert_eq!(
        restart_events
            .iter()
            .filter(|event| event["type"] == "Result")
            .count(),
        1
    );

    #[cfg(target_os = "linux")]
    let stopped = {
        let running: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("data/machines/demo/state.json"))?)?;
        let shim_pid = running["shim_pid"]
            .as_u64()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or("running state has no shim pid")?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(shim_pid),
            nix::sys::signal::Signal::SIGKILL,
        )?;
        thread::sleep(Duration::from_millis(200));
        firestone(&home, &path)
            .args(["--json", "stop", "demo", "--timeout", "2s"])
            .output()?
    };
    #[cfg(not(target_os = "linux"))]
    let stopped = firestone(&home, &path)
        .args(["--json", "stop", "demo", "--force", "--timeout", "2s"])
        .output()?;
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(
        ndjson(&stopped)?.last().ok_or("missing stop result")?["action"],
        "stop"
    );
    let removed = firestone(&home, &path)
        .args(["--json", "rm", "demo"])
        .output()?;
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert_eq!(
        ndjson(&removed)?[0]["payload"]["removed"],
        serde_json::json!(["demo"])
    );

    let image_removed = firestone(&home, &path)
        .args(["--json", "images", "rm", &image_id, "--force"])
        .output()?;
    assert!(image_removed.status.success());
    assert!(
        ndjson(&image_removed)?[0]["payload"]["bytes_freed"]
            .as_u64()
            .is_some()
    );

    let pulled = firestone(&home, &path)
        .arg("--json")
        .args(["images", "pull"])
        .arg(&second_source)
        .output()?;
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    assert_eq!(
        ndjson(&pulled)?.last().ok_or("missing pull result")?["action"],
        "images-pull"
    );
    let pruned = firestone(&home, &path)
        .args(["--json", "images", "prune"])
        .output()?;
    assert!(pruned.status.success());
    assert_eq!(
        ndjson(&pruned)?[0]["payload"]["removed"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );

    let local_sha = firestone(&home, &path)
        .arg("--json")
        .args(["images", "pull"])
        .arg(&source)
        .args(["--sha256", &"a".repeat(64)])
        .output()?;
    assert_eq!(local_sha.status.code(), Some(2));
    assert_eq!(ndjson(&local_sha)?[0]["error"]["kind"], "usage");

    let slow_created = create_fake_machine(
        &home,
        &path,
        "slow",
        &source,
        &firmware,
        &fake_vmm,
        "never-ready",
    )?;
    assert!(slow_created.status.success());
    let absent_vmconfig = firestone(&home, &path)
        .args(["--json", "show", "slow", "--vmconfig"])
        .output()?;
    assert_eq!(absent_vmconfig.status.code(), Some(3));
    let idle_stop = firestone(&home, &path)
        .args(["--json", "stop", "slow"])
        .output()?;
    assert!(idle_stop.status.success());
    let timed_out = firestone(&home, &path)
        .args(["--json", "start", "slow", "--timeout", "1s"])
        .output()?;
    assert_eq!(timed_out.status.code(), Some(6));
    assert!(
        !ndjson(&timed_out)?
            .iter()
            .any(|event| event["type"] == "Result")
    );
    let slow_removed = firestone(&home, &path)
        .args(["--json", "rm", "slow", "--force"])
        .output()?;
    assert!(slow_removed.status.success());
    Ok(())
}
