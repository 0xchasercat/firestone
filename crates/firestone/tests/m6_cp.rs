use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

type TestResult = Result<(), Box<dyn Error>>;

fn firestone(home: &Path, path: &OsString) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.arg("--home").arg(home).env("PATH", path);
    command
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

fn write_program(path: &Path, body: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

struct MachineCleanup {
    home: PathBuf,
    path: OsString,
    name: String,
}

impl Drop for MachineCleanup {
    fn drop(&mut self) {
        let _ = firestone(&self.home, &self.path)
            .args(["stop", &self.name, "--force", "--timeout", "1s"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = firestone(&self.home, &self.path)
            .args(["rm", &self.name, "--force"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn last_call(record: &str) -> Result<String, Box<dyn Error>> {
    Ok(record
        .split("BEGIN\n")
        .filter(|call| !call.is_empty())
        .last()
        .ok_or("missing recorded scp invocation")?
        .to_owned())
}

#[test]
fn m6_cp_cli_smoke_without_kvm() -> TestResult {
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
    let qemu_img = bin.join("qemu-img");
    fs::copy(&fake_vmm, &qemu_img)?;
    fs::set_permissions(&qemu_img, fs::Permissions::from_mode(0o700))?;

    write_program(
        &bin.join("ssh"),
        r#"#!/bin/sh
set -eu
exit "${FAKE_SSH_EXIT-0}"
"#,
    )?;
    let scp_record = root.join("scp-record.log");
    write_program(
        &bin.join("scp"),
        r#"#!/bin/sh
set -eu
: "${FAKE_SCP_RECORD:?}"
{
  printf 'BEGIN\n'
  for argument in "$@"; do
    printf 'ARG=%s\n' "$argument"
  done
  printf 'END\n'
} >> "$FAKE_SCP_RECORD"
exit "${FAKE_SCP_EXIT-0}"
"#,
    )?;

    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;
    let source = root.join("m6-base.qcow2");
    fs::write(&source, b"QFI\xfbM6-CP-SMOKE")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("m6-firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let payload = root.join("payload.txt");
    fs::write(&payload, b"copy me\n")?;
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o600))?;

    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        name: "copy".to_owned(),
    };

    let mut boot = firestone(&home, &path);
    boot.arg("run")
        .arg(&source)
        .args(["--name", "copy", "--net", "none"])
        .arg("--vmm-binary")
        .arg(&fake_vmm)
        .arg("--vmm-firmware")
        .arg(&firmware)
        .env("FAKE_SSH_EXIT", "0");
    for value in [
        "--record".to_owned(),
        root.join("copy-requests.log")
            .to_string_lossy()
            .into_owned(),
        "--body".to_owned(),
        root.join("copy-body.json").to_string_lossy().into_owned(),
        "--behavior".to_owned(),
        "normal".to_owned(),
        "--console-log".to_owned(),
        home.join("data/machines/copy/console.log")
            .to_string_lossy()
            .into_owned(),
    ] {
        boot.arg(format!("--vmm-arg={value}"));
    }
    boot.arg("--").arg("ready");
    let booted: Output = boot.output()?;
    assert!(
        booted.status.success(),
        "run stderr:\n{}",
        String::from_utf8_lossy(&booted.stderr)
    );

    let upload = firestone(&home, &path)
        .arg("cp")
        .arg("-r")
        .arg(&payload)
        .arg("copy:/srv/payload.txt")
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert!(
        upload.status.success(),
        "cp stderr:\n{}",
        String::from_utf8_lossy(&upload.stderr)
    );
    assert!(upload.stdout.is_empty(), "cp leaked a Result to stdout");
    let uploaded = last_call(&fs::read_to_string(&scp_record)?)?;
    let expected_proxy = format!(
        "env FIRESTONE_CONFIG_DIR={} FIRESTONE_DATA_DIR={} FIRESTONE_RUNTIME_DIR={} {} _vsock-proxy copy 22",
        home.join("config").display(),
        home.join("data").display(),
        home.join("run").display(),
        env!("CARGO_BIN_EXE_firestone"),
    );
    assert!(uploaded.starts_with("ARG=-r\n"), "{uploaded}");
    assert!(uploaded.contains(&format!("ARG=ProxyCommand={expected_proxy}")));
    assert!(uploaded.contains(&format!(
        "ARG=IdentityFile={}",
        home.join("data/ssh/id_ed25519").display()
    )));
    assert!(uploaded.contains(&format!(
        "ARG=UserKnownHostsFile={}",
        home.join("data/machines/copy/known_hosts").display()
    )));
    assert!(uploaded.contains("ARG=StrictHostKeyChecking=accept-new\n"));
    assert!(uploaded.contains("ARG=LogLevel=ERROR\n"));
    assert!(!uploaded.contains("ARG=BatchMode=yes\n"));
    assert!(uploaded.ends_with(&format!(
        "ARG={}\nARG=root@firestone.copy:/srv/payload.txt\nEND\n",
        payload.display()
    )));

    let download = firestone(&home, &path)
        .args(["cp", "copy:/etc/hostname", "./Host:name"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert!(
        download.status.success(),
        "cp stderr:\n{}",
        String::from_utf8_lossy(&download.stderr)
    );
    let downloaded = last_call(&fs::read_to_string(&scp_record)?)?;
    assert!(!downloaded.contains("ARG=-r\n"));
    assert!(downloaded.ends_with("ARG=root@firestone.copy:/etc/hostname\nARG=./Host:name\nEND\n",));

    let ambiguous = firestone(&home, &path)
        .args(["cp", "copy:/etc/hostname", "Host:name"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert!(ambiguous.status.success());
    let escaped = last_call(&fs::read_to_string(&scp_record)?)?;
    assert!(escaped.ends_with("ARG=./Host:name\nEND\n"), "{escaped}");

    let exit_code = firestone(&home, &path)
        .args(["cp", "./payload.txt", "copy:/srv/payload.txt"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .env("FAKE_SCP_EXIT", "17")
        .output()?;
    assert_eq!(exit_code.status.code(), Some(17));

    let json = firestone(&home, &path)
        .args(["--json", "cp", "./payload.txt", "copy:/srv/payload.txt"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert_eq!(json.status.code(), Some(2));
    let event: serde_json::Value = serde_json::from_str(String::from_utf8(json.stdout)?.trim())?;
    assert_eq!(event["error"]["kind"], "usage");

    let two_local = firestone(&home, &path)
        .args(["cp", "./payload.txt", "./other.txt"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert_eq!(two_local.status.code(), Some(2));
    assert!(
        String::from_utf8(two_local.stderr)?.contains("neither cp operand names a machine"),
        "unexpected usage error"
    );

    let before = fs::read_to_string(&scp_record)?;
    let stopped = firestone(&home, &path)
        .args(["stop", "copy", "--force", "--timeout", "2s"])
        .output()?;
    assert!(stopped.status.success());
    let refused = firestone(&home, &path)
        .args(["cp", "./payload.txt", "copy:/srv/payload.txt"])
        .current_dir(&root)
        .env("FAKE_SCP_RECORD", &scp_record)
        .output()?;
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8(refused.stderr)?;
    assert!(stderr.contains("machine copy is not running"), "{stderr}");
    assert!(stderr.contains("firestone start copy"), "{stderr}");
    assert_eq!(
        fs::read_to_string(&scp_record)?,
        before,
        "cp ran scp anyway"
    );
    Ok(())
}
