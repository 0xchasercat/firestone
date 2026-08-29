use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

use nix::{
    poll::{PollFd, PollFlags, poll},
    pty::openpty,
    sys::{
        signal::{Signal, kill},
        termios::{LocalFlags, tcgetattr},
    },
    unistd::Pid,
};
use std::os::fd::AsFd as _;
use wait_timeout::ChildExt as _;

const CONSOLE_CONNECTED: &[u8] = "connected to m2 console · escape: Ctrl-]".as_bytes();

type TestResult = Result<(), Box<dyn Error>>;

fn firestone(home: &Path, path: &OsString) -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", "umask 002; exec \"$@\"", "firestone-test"])
        .arg(env!("CARGO_BIN_EXE_firestone"))
        .arg("--home")
        .arg(home)
        .env("PATH", path);
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

struct RunFixture<'a> {
    home: &'a Path,
    path: &'a OsString,
    root: &'a Path,
    source: &'a Path,
    firmware: &'a Path,
    fake_vmm: &'a Path,
    ssh_record: &'a Path,
}

fn invoke_run(
    fixture: &RunFixture<'_>,
    name: &str,
    remove: bool,
    remote: &[&str],
    exit: &str,
) -> Result<Output, Box<dyn Error>> {
    let record = fixture.root.join(format!("{name}-requests.log"));
    let body = fixture.root.join(format!("{name}-body.json"));
    let console = fixture
        .home
        .join("data/machines")
        .join(name)
        .join("console.log");
    let mut command = firestone(fixture.home, fixture.path);
    command
        .arg("run")
        .arg(fixture.source)
        .arg("--name")
        .arg(name)
        .arg("--net")
        .arg("none")
        .arg("--vmm-binary")
        .arg(fixture.fake_vmm)
        .arg("--vmm-firmware")
        .arg(fixture.firmware)
        .env("FAKE_SSH_RECORD", fixture.ssh_record)
        .env("FAKE_SSH_EXIT", exit)
        .env("FIRESTONE_RUN_PROXY", "1")
        .env("SSH_TEST_MARKER", "visible");
    if remove {
        command.arg("--rm");
    }
    for value in [
        "--record".to_owned(),
        record.to_string_lossy().into_owned(),
        "--body".to_owned(),
        body.to_string_lossy().into_owned(),
        "--behavior".to_owned(),
        "normal".to_owned(),
        "--console-log".to_owned(),
        console.to_string_lossy().into_owned(),
    ] {
        command.arg(format!("--vmm-arg={value}"));
    }
    if !remote.is_empty() {
        command.arg("--").args(remote);
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

struct ConsoleAttach {
    output: Vec<u8>,
    status: ExitStatus,
}

fn attach_console(
    home: &Path,
    path: &OsString,
    name: &str,
    signal: bool,
) -> Result<ConsoleAttach, Box<dyn Error>> {
    let opened = openpty(None, None)?;
    let mut master = fs::File::from(opened.master);
    let terminal = fs::File::from(opened.slave);
    let original = tcgetattr(&terminal)?;
    let mut command = firestone(home, path);
    command
        .args(["console", name])
        .stdin(Stdio::from(terminal.try_clone()?))
        .stdout(Stdio::from(terminal.try_clone()?))
        .stderr(Stdio::from(terminal.try_clone()?));
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut output = Vec::new();
    while !output
        .windows(CONSOLE_CONNECTED.len())
        .any(|window| window == CONSOLE_CONNECTED)
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("console connection line timed out".into());
        }
        let ready = {
            let mut descriptors = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
            poll(&mut descriptors, 100_u16)? > 0
        };
        if ready {
            let mut bytes = [0_u8; 4096];
            let read = master.read(&mut bytes)?;
            if read == 0 {
                break;
            }
            output.extend_from_slice(&bytes[..read]);
        }
    }
    if signal {
        let pid = i32::try_from(child.id()).map_err(|_| "console child pid overflowed i32")?;
        kill(Pid::from_raw(pid), Signal::SIGTERM)?;
    } else {
        master.write_all(&[0x1d])?;
    }
    let status = match child.wait_timeout(Duration::from_secs(3))? {
        Some(status) => status,
        None => {
            child.kill()?;
            let _ = child.wait();
            return Err("console did not detach within its deadline".into());
        }
    };
    let restored = tcgetattr(&terminal)?;
    assert!(restored.local_flags.contains(LocalFlags::ICANON));
    assert!(restored.local_flags.contains(LocalFlags::ECHO));
    assert_eq!(restored.input_flags, original.input_flags);
    assert_eq!(restored.output_flags, original.output_flags);
    assert_eq!(restored.control_flags, original.control_flags);
    assert_eq!(restored.control_chars, original.control_chars);
    Ok(ConsoleAttach { output, status })
}

fn interactive_shell(
    home: &Path,
    path: &OsString,
    record: &Path,
) -> Result<ExitStatus, Box<dyn Error>> {
    let opened = openpty(None, None)?;
    let _master = fs::File::from(opened.master);
    let terminal = fs::File::from(opened.slave);
    let mut command = firestone(home, path);
    command
        .args(["shell", "m2", "--", "interactive-start"])
        .env("FAKE_SSH_RECORD", record)
        .env("FAKE_SSH_EXIT", "0")
        .stdin(Stdio::from(terminal.try_clone()?))
        .stdout(Stdio::from(terminal.try_clone()?))
        .stderr(Stdio::from(terminal));
    let mut child = command.spawn()?;
    match child.wait_timeout(Duration::from_secs(20))? {
        Some(status) => Ok(status),
        None => {
            child.kill()?;
            let _ = child.wait();
            Err("interactive shell did not finish within its deadline".into())
        }
    }
}

#[test]
fn m2_terminal_cli_smoke_without_kvm() -> TestResult {
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

    let ssh_record = root.join("ssh-record.log");
    let fake_ssh = bin.join("ssh");
    fs::write(
        &fake_ssh,
        r#"#!/bin/sh
set -eu
: "${FAKE_SSH_RECORD:?}"
last=''
proxy=''
{
  printf 'BEGIN\n'
  printf 'ENV=%s\n' "${SSH_TEST_MARKER-}"
  for argument in "$@"; do
    printf 'ARG=%s\n' "$argument"
    last=$argument
    case "$argument" in
      ProxyCommand=*) proxy=${argument#ProxyCommand=} ;;
    esac
  done
  printf 'END\n'
} >> "$FAKE_SSH_RECORD"
if [ "${FIRESTONE_RUN_PROXY-}" = 1 ]; then
  : "${proxy:?missing ProxyCommand}"
  sh -c "$proxy"
fi
if [ "$last" = true ]; then exit 0; fi
if [ "$last" = signal-run ]; then kill -TERM $$; fi
if [ "${FAKE_SSH_SIGNAL-}" = TERM ]; then kill -TERM $$; fi
exit "${FAKE_SSH_EXIT-0}"
"#,
    )?;
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700))?;

    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;
    let source = root.join("m2-base.qcow2");
    fs::write(&source, b"QFI\xfbM2-CLI-SMOKE")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("m2-firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        names: vec![
            "m2".to_owned(),
            "ephemeral".to_owned(),
            "signal-runner".to_owned(),
            "cancel".to_owned(),
            "prompt".to_owned(),
        ],
    };
    let fixture = RunFixture {
        home: &home,
        path: &path,
        root: &root,
        source: &source,
        firmware: &firmware,
        fake_vmm: &fake_vmm,
        ssh_record: &ssh_record,
    };

    let run = invoke_run(
        &fixture,
        "m2",
        false,
        &["printf one argument", "--remote-flag"],
        "37",
    )?;
    assert_eq!(
        run.status.code(),
        Some(37),
        "run stderr:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.stdout.is_empty(),
        "run leaked intermediate Results to stdout"
    );
    let run_stderr = String::from_utf8(run.stderr)?;
    assert!(run_stderr.contains("[boot]"));
    assert!(run_stderr.contains("[ssh]"));
    assert!(home.join("data/machines/m2").is_dir());

    let ssh_calls = fs::read_to_string(&ssh_record)?;
    assert!(ssh_calls.contains("ARG=BatchMode=yes"));
    assert!(ssh_calls.contains("ARG=true"));
    assert!(ssh_calls.contains("ARG=printf one argument"));
    assert!(ssh_calls.contains("ARG=--remote-flag"));
    assert!(ssh_calls.contains("ENV=visible"));
    assert!(!ssh_calls.contains("ARG=-t\n"));
    let expected_proxy = format!(
        "FIRESTONE_CONFIG_DIR={} FIRESTONE_DATA_DIR={} FIRESTONE_RUNTIME_DIR={} {} _vsock-proxy m2 22",
        home.join("config").display(),
        home.join("data").display(),
        home.join("run").display(),
        env!("CARGO_BIN_EXE_firestone"),
    );
    assert!(ssh_calls.contains(&format!("ARG=ProxyCommand={expected_proxy}")));
    assert!(ssh_calls.contains(&format!(
        "ARG=IdentityFile={}",
        home.join("data/ssh/id_ed25519").display()
    )));
    assert!(ssh_calls.contains(&format!(
        "ARG=UserKnownHostsFile={}",
        home.join("data/machines/m2/known_hosts").display()
    )));

    let config = firestone(&home, &path)
        .args(["ssh-config", "m2"])
        .output()?;
    assert!(config.status.success());
    assert!(config.stderr.is_empty());
    let expected_config = format!(
        "Host firestone.m2\n  User root\n  ProxyCommand {expected_proxy}\n  IdentityFile {}\n  IdentitiesOnly yes\n  UserKnownHostsFile {}\n  StrictHostKeyChecking accept-new\n",
        home.join("data/ssh/id_ed25519").display(),
        home.join("data/machines/m2/known_hosts").display(),
    );
    assert_eq!(String::from_utf8(config.stdout)?, expected_config);
    let json_config = firestone(&home, &path)
        .args(["--json", "ssh-config", "m2"])
        .output()?;
    let config_events = ndjson(&json_config)?;
    assert_eq!(config_events.len(), 1);
    assert_eq!(config_events[0]["type"], "Result");
    assert_eq!(config_events[0]["action"], "ssh-config");
    assert_eq!(config_events[0]["payload"]["config"], expected_config);

    let signalled = firestone(&home, &path)
        .args(["shell", "m2", "--", "signal-me"])
        .env("FAKE_SSH_RECORD", &ssh_record)
        .env("FAKE_SSH_SIGNAL", "TERM")
        .output()?;
    assert_eq!(signalled.status.signal(), Some(15));

    let known_hosts = home.join("data/machines/m2/known_hosts");
    fs::write(&known_hosts, b"hard-change-evidence\n")?;
    fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600))?;
    let hard_change = firestone(&home, &path)
        .args(["shell", "m2", "--", "hard-change"])
        .env("FAKE_SSH_RECORD", &ssh_record)
        .env("FAKE_SSH_EXIT", "255")
        .output()?;
    assert_eq!(hard_change.status.code(), Some(255));
    assert_eq!(fs::read(&known_hosts)?, b"hard-change-evidence\n");

    let non_tty_console = firestone(&home, &path).args(["console", "m2"]).output()?;
    assert_eq!(non_tty_console.status.code(), Some(2));
    assert!(non_tty_console.stdout.is_empty());
    assert!(String::from_utf8(non_tty_console.stderr)?.contains("console requires terminal"));
    for _ in 0..2 {
        let attached = attach_console(&home, &path, "m2", false)?;
        assert!(attached.status.success());
        assert!(
            attached
                .output
                .windows(CONSOLE_CONNECTED.len())
                .any(|window| window == CONSOLE_CONNECTED)
        );
    }
    let interrupted = attach_console(&home, &path, "m2", true)?;
    assert_eq!(interrupted.status.code(), Some(130));

    let json_console = firestone(&home, &path)
        .args(["--json", "console", "m2"])
        .output()?;
    assert!(json_console.stderr.is_empty());
    let console_events = ndjson(&json_console)?;
    assert_eq!(console_events.len(), 1);
    assert_eq!(console_events[0]["error"]["kind"], "usage");

    let reused = firestone(&home, &path)
        .args(["run", "m2", "--rm", "--", "reuse"])
        .env("FAKE_SSH_RECORD", &ssh_record)
        .env("FAKE_SSH_EXIT", "0")
        .output()?;
    assert!(reused.status.success());
    assert!(
        home.join("data/machines/m2").is_dir(),
        "--rm removed a reused machine"
    );

    let stopped = firestone(&home, &path)
        .args(["stop", "m2", "--force", "--timeout", "2s"])
        .output()?;
    assert!(stopped.status.success());
    let noninteractive = firestone(&home, &path)
        .args(["shell", "m2", "--", "must-not-start"])
        .env("FAKE_SSH_RECORD", &ssh_record)
        .output()?;
    assert_eq!(noninteractive.status.code(), Some(1));
    assert!(String::from_utf8(noninteractive.stderr)?.contains("machine m2 is not running"));
    let interactive = interactive_shell(&home, &path, &ssh_record)?;
    assert!(interactive.success());
    assert!(fs::read_to_string(&ssh_record)?.contains("ARG=-t\n"));

    let signal_removed = invoke_run(&fixture, "signal-runner", true, &["signal-run"], "0")?;
    assert_eq!(signal_removed.status.signal(), Some(15));
    assert!(!home.join("data/machines/signal-runner").exists());

    let cancel_state = home.join("data/machines/cancel/state.json");
    let mut cancel_command = firestone(&home, &path);
    cancel_command
        .arg("run")
        .arg(&source)
        .args(["--name", "cancel", "--net", "none"])
        .arg("--vmm-binary")
        .arg(&fake_vmm)
        .arg("--vmm-firmware")
        .arg(&firmware)
        .env("FAKE_SSH_RECORD", &ssh_record)
        .env("FAKE_SSH_EXIT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for value in [
        "--record".to_owned(),
        root.join("cancel-requests.log")
            .to_string_lossy()
            .into_owned(),
        "--body".to_owned(),
        root.join("cancel-body.json").to_string_lossy().into_owned(),
        "--behavior".to_owned(),
        "never-ready".to_owned(),
    ] {
        cancel_command.arg(format!("--vmm-arg={value}"));
    }
    let mut cancelling = cancel_command.spawn()?;
    let start_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = fs::read(&cancel_state)
            && serde_json::from_slice::<serde_json::Value>(&bytes)?["status"] == "starting"
        {
            break;
        }
        if Instant::now() >= start_deadline {
            cancelling.kill()?;
            let _ = cancelling.wait();
            return Err("cancel smoke did not enter starting state".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let cancelling_pid =
        i32::try_from(cancelling.id()).map_err(|_| "cancel child pid overflowed i32")?;
    kill(Pid::from_raw(cancelling_pid), Signal::SIGINT)?;
    let cancelled = cancelling
        .wait_timeout(Duration::from_secs(12))?
        .ok_or("cancelled start did not exit within its deadline")?;
    assert_eq!(cancelled.code(), Some(130));
    let cancelled_state: serde_json::Value = serde_json::from_slice(&fs::read(&cancel_state)?)?;
    assert_eq!(cancelled_state["status"], "created");
    assert!(!home.join("run/cancel").exists());

    let removed = invoke_run(&fixture, "ephemeral", true, &["exit-23"], "23")?;
    assert_eq!(removed.status.code(), Some(23));
    assert!(removed.stdout.is_empty());
    assert!(!home.join("data/machines/ephemeral").exists());

    fs::write(&ssh_record, b"")?;
    let prompt = invoke_run(&fixture, "prompt", true, &[], "0")?;
    assert!(prompt.status.success());
    assert!(!home.join("data/machines/prompt").exists());
    let prompt_calls = fs::read_to_string(&ssh_record)?;
    let final_call = prompt_calls
        .split("BEGIN\n")
        .filter(|call| !call.is_empty())
        .last()
        .ok_or("missing prompt SSH invocation")?;
    assert!(final_call.contains("ARG=root@firestone.prompt\n"));
    assert!(!final_call.contains("ARG=true\n"));
    assert!(final_call.ends_with("ARG=root@firestone.prompt\nEND\n"));
    Ok(())
}
