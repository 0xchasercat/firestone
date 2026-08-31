use std::{
    env,
    error::Error,
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::{fd::AsFd as _, unix::fs::PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use nix::{
    poll::{PollFd, PollFlags, poll},
    pty::{Winsize, openpty},
};
use wait_timeout::ChildExt as _;

type TestResult = Result<(), Box<dyn Error>>;

fn write_test_catalog(home: &Path) -> TestResult {
    let config = home.join("config");
    fs::create_dir_all(&config)?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700))?;
    fs::write(
        config.join("catalog.toml"),
        r#"[[image]]
distro = "alpine"
version = "3.22"
aliases = ["edge"]
default = true
firmware = "edk2"
format = "qcow2"
sshd_path = "/usr/sbin/sshd"

[image.arch.x86_64]
url = "https://example.invalid/alpine-x86_64.qcow2"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[image.arch.aarch64]
firmware = "rhf"
url = "https://example.invalid/alpine-aarch64.qcow2"
sha256 = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
"#,
    )?;
    Ok(())
}

#[test]
fn missing_command_human_emits_readable_help_with_usage_exit() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone")).output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with(
        "Firestone's command-line interface\n\nUsage: firestone [OPTIONS] <COMMAND>\n\nCommands:\n"
    ));
    assert!(stderr.contains("\n  run          Create or reuse a machine"));
    assert!(stderr.contains("\nOptions:\n"));
    assert!(!stderr.contains('\u{fffd}'));
    Ok(())
}

#[test]
fn missing_command_json_requested_emits_structured_usage_error() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .arg("--json")
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["kind"], "usage");
    assert_eq!(
        value["error"]["message"],
        "'firestone' requires a subcommand but one was not provided"
    );
    Ok(())
}
#[test]
fn invalid_argument_human_emits_single_line_error_and_hint() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .arg("--bogus")
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr)?,
        "error: unexpected argument '--bogus' found\nhint:  run `firestone --help` for command usage\n"
    );
    Ok(())
}
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
fn stdout_broken_pipe_is_a_normal_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let (reader, writer) = nix::unistd::pipe()?;
    drop(reader);

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--home"])
        .arg(&home)
        .arg("version")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()?;

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
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

struct PtyOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    terminal_restored: bool,
}

struct PtyCommand {
    child: Option<Child>,
    terminal: Option<fs::File>,
    input: Option<fs::File>,
    original_termios: nix::sys::termios::Termios,
    stdout: Option<ChildStdout>,
    reader: Option<thread::JoinHandle<std::io::Result<Vec<u8>>>>,
    reader_done: Arc<AtomicBool>,
}

impl PtyCommand {
    fn spawn(
        command: Command,
        rows: u16,
        columns: u16,
        term: &str,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_stdin(command, rows, columns, term, false)
    }

    fn spawn_interactive(
        command: Command,
        rows: u16,
        columns: u16,
        term: &str,
    ) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_stdin(command, rows, columns, term, true)
    }

    fn spawn_with_stdin(
        mut command: Command,
        rows: u16,
        columns: u16,
        term: &str,
        interactive: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let size = Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let opened = openpty(Some(&size), None)?;
        let mut master = fs::File::from(opened.master);
        let terminal = fs::File::from(opened.slave);
        let input = if interactive {
            Some(master.try_clone()?)
        } else {
            None
        };
        let original_termios = nix::sys::termios::tcgetattr(&terminal)?;
        command.env("TERM", term);
        if interactive {
            command.stdin(Stdio::from(terminal.try_clone()?));
        } else {
            command.stdin(Stdio::null());
        }
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::from(terminal.try_clone()?));
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or("PTY command stdout is not piped")?;
        drop(command);
        let reader_done = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&reader_done);
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            loop {
                let (ready, events) = {
                    let mut descriptors = [PollFd::new(
                        master.as_fd(),
                        PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                    )];
                    let ready = poll(&mut descriptors, 50_u16)?;
                    (
                        ready,
                        descriptors[0].revents().unwrap_or_else(PollFlags::empty),
                    )
                };
                if events.contains(PollFlags::POLLIN) {
                    let mut chunk = [0_u8; 4096];
                    match master.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                        Err(source) if source.raw_os_error() == Some(nix::libc::EIO) => break,
                        Err(source) => return Err(source),
                    }
                    continue;
                }
                if events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR)
                    || (ready == 0 && done.load(Ordering::Relaxed))
                {
                    break;
                }
            }
            Ok(bytes)
        });
        Ok(Self {
            child: Some(child),
            terminal: Some(terminal),
            input,
            original_termios,
            stdout: Some(stdout),
            reader: Some(reader),
            reader_done,
        })
    }

    fn write_input(&mut self, value: &[u8]) -> Result<(), Box<dyn Error>> {
        let input = self
            .input
            .as_mut()
            .ok_or("PTY command has no interactive input")?;
        input.write_all(value)?;
        input.flush()?;
        Ok(())
    }

    fn resize(&self, rows: u16, columns: u16) -> Result<(), Box<dyn Error>> {
        let terminal = self
            .terminal
            .as_ref()
            .ok_or("PTY terminal is already closed")?;
        let output = Command::new("stty")
            .arg("rows")
            .arg(rows.to_string())
            .arg("cols")
            .arg(columns.to_string())
            .stdin(Stdio::from(terminal.try_clone()?))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "cannot resize PTY: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }

    fn signal(&self, signal: nix::sys::signal::Signal) -> Result<(), Box<dyn Error>> {
        let child = self.child.as_ref().ok_or("PTY command already exited")?;
        let pid = i32::try_from(child.id()).map_err(|_| "PTY child pid overflowed i32")?;
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal)?;
        Ok(())
    }

    fn finish(mut self, timeout: Duration) -> Result<PtyOutput, Box<dyn Error>> {
        let mut child = self.child.take().ok_or("PTY command already exited")?;
        let status = match child.wait_timeout(timeout)? {
            Some(status) => status,
            None => {
                child.kill()?;
                let _ = child.wait();
                self.terminal.take();
                return Err("PTY command did not exit before its deadline".into());
            }
        };
        let current_termios = nix::sys::termios::tcgetattr(
            self.terminal
                .as_ref()
                .ok_or("PTY terminal is already closed")?,
        )?;
        let terminal_restored = terminal_modes_equal(&self.original_termios, &current_termios);
        self.terminal.take();

        let mut stdout = Vec::new();
        let mut stdout_pipe = self
            .stdout
            .take()
            .ok_or("PTY command stdout is unavailable")?;
        loop {
            let (ready, events) = {
                let mut descriptors = [PollFd::new(
                    stdout_pipe.as_fd(),
                    PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
                )];
                let ready = poll(&mut descriptors, 50_u16)?;
                (
                    ready,
                    descriptors[0].revents().unwrap_or_else(PollFlags::empty),
                )
            };
            if events.contains(PollFlags::POLLIN) {
                let mut chunk = [0_u8; 4096];
                let read = stdout_pipe.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                stdout.extend_from_slice(&chunk[..read]);
                continue;
            }
            if ready == 0 || events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
                break;
            }
        }
        self.reader_done.store(true, Ordering::Relaxed);
        let stderr = self
            .reader
            .take()
            .ok_or("PTY reader is unavailable")?
            .join()
            .map_err(|_| "PTY reader panicked")??;
        Ok(PtyOutput {
            status,
            stdout,
            stderr,
            terminal_restored,
        })
    }
}

impl Drop for PtyCommand {
    fn drop(&mut self) {
        self.reader_done.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.terminal.take();
    }
}

fn terminal_modes_equal(
    left: &nix::sys::termios::Termios,
    right: &nix::sys::termios::Termios,
) -> bool {
    left.input_flags == right.input_flags
        && left.output_flags == right.output_flags
        && left.control_flags == right.control_flags
        && left.local_flags == right.local_flags
        && left.control_chars == right.control_chars
}

#[test]
fn create_non_tty_renders_effective_summary_without_prompting() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let share = root.join("share");
    fs::create_dir(&share)?;
    let mount = format!("{}:/work:ro", share.display());

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--home"])
        .arg(&home)
        .args([
            "create",
            "dev",
            "ubuntu",
            "--cpus",
            "1",
            "--memory",
            "4G",
            "--disk",
            "30G",
            "--net",
            "passt",
            "--forward",
            "8080:80",
            "--mount",
        ])
        .arg(&mount)
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = format!(
        "Created machine\n  Name: dev\n  Image: ubuntu:24.04\n  CPUs: 1\n  Memory: 4G\n  Disk: 30G\n  Network: passt\n  Forwards:\n    8080:80\n  Mounts:\n    {} -> /work (read-only, tag share0)\n  Config: {}\nEdit: firestone edit dev\nStart: firestone start dev\n",
        share.display(),
        home.join("data/machines/dev/firestone.toml").display(),
    );
    assert_eq!(String::from_utf8(output.stdout)?, expected);
    assert!(!String::from_utf8(output.stderr)?.contains("Create a machine"));
    Ok(())
}

#[test]
fn create_non_tty_without_image_preserves_usage_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--home"])
        .arg(&home)
        .arg("create")
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("image is required"));
    assert!(!stderr.contains("Create a machine"));
    Ok(())
}

#[test]
fn create_json_preserves_machine_record_payload() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--json", "--home"])
        .arg(&home)
        .args(["create", "json-dev", "ubuntu", "--cpus", "1"])
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let records = ndjson(&output)?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "Result");
    assert_eq!(records[0]["action"], "create");
    assert_eq!(records[0]["payload"]["name"], "json-dev");
    assert_eq!(records[0]["payload"]["spec"]["image"], "ubuntu:24.04");
    assert_eq!(records[0]["payload"]["spec"]["cpus"], 1);
    assert!(!String::from_utf8(output.stdout)?.contains("Created machine"));
    Ok(())
}

#[test]
fn create_quiet_keeps_final_result_and_hides_feedback() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--quiet", "--home"])
        .arg(&home)
        .args(["create", "quiet-dev", "ubuntu"])
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("Created machine\n  Name: quiet-dev\n"));
    assert!(stdout.contains("Start: firestone start quiet-dev\n"));
    Ok(())
}

#[test]
fn catalog_lists_merged_entries_for_human_and_json_callers() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    write_test_catalog(&home)?;

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--home"])
        .arg(&home)
        .arg("catalog")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("REFERENCE"));
    let alpine = stdout
        .lines()
        .find(|line| line.starts_with("alpine:3.22"))
        .ok_or("merged Alpine catalog row is missing")?;
    assert!(alpine.contains("edge"));
    assert!(alpine.contains("aarch64=rhf, x86_64=edk2"));
    assert!(alpine.contains("aarch64, x86_64"));
    assert!(stdout.contains("ubuntu:24.04"));
    assert!(stdout.contains("noble"));

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--json", "--home"])
        .arg(&home)
        .arg("catalog")
        .output()?;
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["type"], "Result");
    assert_eq!(result["action"], "catalog");
    let alpine = result["payload"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["reference"] == "alpine:3.22")
        })
        .ok_or("merged Alpine JSON entry is missing")?;
    assert_eq!(alpine["aliases"], serde_json::json!(["edge"]));
    assert_eq!(
        alpine["architectures"],
        serde_json::json!([
            {"architecture":"aarch64","firmware":"rhf"},
            {"architecture":"x86_64","firmware":"edk2"}
        ])
    );
    Ok(())
}

#[test]
fn create_help_explains_wizard_and_spec_options() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["create", "--help"])
        .output()?;

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("On a terminal, create prompts for the image"));
    assert!(help.contains("--yes"));
    assert!(help.contains("Set the number of virtual CPUs"));
    assert!(help.contains("Forward a host port or range to the guest"));
    assert!(help.contains("Merge a JSON object into the generated VMM configuration"));
    Ok(())
}

#[test]
fn create_tty_runs_wizard_with_effective_defaults() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.args(["--home"]).arg(&home).arg("create");

    let mut process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    process.write_input(b"\nwizard\n1\n4G\n30G\nnone\n")?;
    let output = process.finish(Duration::from_secs(15))?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.terminal_restored);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("Create a machine. Choose an image with arrow keys and Enter"));
    assert!(stderr.contains("Image: "));
    assert!(stderr.contains("Name [ubuntu]: "));
    assert!(stderr.contains("CPUs [2]: "));
    assert!(stderr.contains("Memory [2G]: "));
    assert!(stderr.contains("Disk [20G]: "));
    assert!(stderr.contains("Network [passt]: "));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("  Name: wizard\n"));
    assert!(stdout.contains("  Image: ubuntu:24.04\n"));
    assert!(stdout.contains("  CPUs: 1\n"));
    assert!(stdout.contains("  Memory: 4G\n"));
    assert!(stdout.contains("  Disk: 30G\n"));
    assert!(stdout.contains("  Network: none\n"));
    assert!(home.join("data/machines/wizard/firestone.toml").is_file());
    Ok(())
}

#[test]
fn create_tty_selects_user_catalog_entry() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    write_test_catalog(&home)?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.args(["--home"]).arg(&home).arg("create");

    let mut process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    process.write_input(b"jj\nalpine-dev\n2\n2G\n20G\npasst\n")?;
    let output = process.finish(Duration::from_secs(15))?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.terminal_restored);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("alpine:3.22 (edge)"));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("  Name: alpine-dev\n"));
    assert!(stdout.contains("  Image: alpine:3.22\n"));
    Ok(())
}

#[test]
fn create_tty_custom_image_option_accepts_url() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.args(["--home"]).arg(&home).arg("create");

    let mut process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    process
        .write_input(b"j\nhttps://example.invalid/custom.qcow2\ncustom-dev\n2\n2G\n20G\nnone\n")?;
    let output = process.finish(Duration::from_secs(15))?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.terminal_restored);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("[Custom URL or local path...]"));
    assert!(stderr.contains("Custom image: "));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("  Image: https://example.invalid/custom.qcow2\n"));
    Ok(())
}

#[test]
fn create_tty_yes_bypasses_wizard() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command
        .args(["--yes", "--home"])
        .arg(&home)
        .args(["create", "yes-dev", "ubuntu"]);

    let process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    let output = process.finish(Duration::from_secs(15))?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.terminal_restored);
    assert!(String::from_utf8(output.stderr)?.is_empty());
    assert!(String::from_utf8(output.stdout)?.contains("  Name: yes-dev\n"));
    Ok(())
}

#[test]
fn create_tty_cancel_returns_interrupted_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.args(["--home"]).arg(&home).arg("create");

    let mut process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    process.write_input(b"\ncancel\n")?;
    let output = process.finish(Duration::from_secs(15))?;

    assert_eq!(output.status.code(), Some(130));
    assert!(output.stdout.is_empty());
    assert!(output.terminal_restored);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("error: create cancelled"));
    assert!(stderr.contains("hint:  run firestone create again when you are ready"));
    assert!(!home.join("data/machines").exists());
    Ok(())
}

#[test]
fn create_tty_eof_returns_interrupted_error() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_firestone"));
    command.args(["--home"]).arg(&home).arg("create");

    let mut process = PtyCommand::spawn_interactive(command, 24, 100, "xterm-256color")?;
    process.write_input(b"\n\x04")?;
    let output = process.finish(Duration::from_secs(15))?;
    assert_eq!(output.status.code(), Some(130));
    assert!(output.stdout.is_empty());
    assert!(output.terminal_restored);
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("error: create wizard received end of input"));
    assert!(stderr.contains("use --yes with explicit arguments"));
    assert!(!home.join("data/machines").exists());
    Ok(())
}

#[test]
fn tty_progress_cli_contract_without_kvm() -> TestResult {
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
    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;
    let source = root.join("progress-base.qcow2");
    fs::write(&source, b"QFI\xfbTTY-PROGRESS")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("progress-firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let names = [
        ("animated", "delayed-ready"),
        ("api-error", "create-fail"),
        ("timeout", "normal"),
        ("interrupt", "delayed-ready"),
    ];
    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        names: names.iter().map(|(name, _)| (*name).to_owned()).collect(),
    };
    for (name, behavior) in names {
        let output =
            create_fake_machine(&home, &path, name, &source, &firmware, &fake_vmm, behavior)?;
        require_success(&output, &format!("create {name}"))?;
    }

    let mut animated_command = firestone(&home, &path);
    animated_command
        .args(["start", "animated", "--no-wait", "--timeout", "20s"])
        .env_remove("NO_COLOR");
    let animated = PtyCommand::spawn(animated_command, 12, 38, "xterm-256color")?;
    thread::sleep(Duration::from_millis(250));
    animated.resize(30, 100)?;
    let animated = animated.finish(Duration::from_secs(20))?;
    assert!(animated.status.success());
    assert_eq!(animated.stdout, b"animated is running\n");
    assert!(animated.terminal_restored);
    let animated_stderr = String::from_utf8_lossy(&animated.stderr);
    let distinct_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        .into_iter()
        .filter(|frame| animated_stderr.contains(frame))
        .count();
    assert!(
        distinct_frames >= 2,
        "expected animated spinner frames, stderr was {animated_stderr:?}"
    );
    assert!(animated_stderr.contains('✓'));
    assert!(animated.stderr.windows(5).any(|bytes| bytes == b"\x1b[32m"));

    let mut api_error_command = firestone(&home, &path);
    api_error_command
        .args(["start", "api-error", "--no-wait", "--timeout", "20s"])
        .env_remove("NO_COLOR");
    let api_error = PtyCommand::spawn(api_error_command, 24, 100, "xterm-256color")?
        .finish(Duration::from_secs(15))?;
    let api_error_stderr = String::from_utf8_lossy(&api_error.stderr);
    let api_error_tail = api_error_stderr
        .chars()
        .rev()
        .take(2_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    assert_eq!(
        api_error.status.code(),
        Some(1),
        "API failure stderr tail was {api_error_tail:?}"
    );
    assert!(api_error.stdout.is_empty());
    assert!(api_error.terminal_restored);
    assert!(api_error_stderr.contains('✗'));
    assert!(api_error_stderr.contains("HTTP status 500"));
    assert!(api_error_stderr.contains("injected create failure"));

    let mut timeout_command = firestone(&home, &path);
    timeout_command.args(["start", "timeout", "--timeout", "1s"]);
    let timeout = PtyCommand::spawn(timeout_command, 24, 100, "xterm-256color")?
        .finish(Duration::from_secs(15))?;
    assert_eq!(timeout.status.code(), Some(6));
    assert!(timeout.stdout.is_empty());
    assert!(timeout.terminal_restored);
    let timeout_stderr = String::from_utf8_lossy(&timeout.stderr);
    assert!(timeout_stderr.contains('✗'));
    assert!(timeout_stderr.contains("timed out") || timeout_stderr.contains("deadline"));

    let mut interrupt_command = firestone(&home, &path);
    interrupt_command.args(["start", "interrupt", "--timeout", "20s"]);
    let interrupt = PtyCommand::spawn(interrupt_command, 24, 100, "xterm-256color")?;
    thread::sleep(Duration::from_millis(250));
    interrupt.signal(nix::sys::signal::Signal::SIGINT)?;
    let interrupt = interrupt.finish(Duration::from_secs(15))?;
    assert_eq!(interrupt.status.code(), Some(130));
    assert!(interrupt.stdout.is_empty());
    assert!(interrupt.terminal_restored);
    assert!(
        !interrupt
            .stderr
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l")
    );
    assert!(String::from_utf8_lossy(&interrupt.stderr).contains("interrupted"));

    let make_source = |name: &str| -> Result<PathBuf, Box<dyn Error>> {
        let path = root.join(format!("{name}.qcow2"));
        let mut bytes = b"QFI\xfb".to_vec();
        bytes.extend_from_slice(name.as_bytes());
        fs::write(&path, bytes)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(path)
    };

    let no_color_source = make_source("no-color")?;
    let mut no_color_command = firestone(&home, &path);
    no_color_command
        .args(["images", "pull"])
        .arg(&no_color_source)
        .env("NO_COLOR", "1");
    let no_color = PtyCommand::spawn(no_color_command, 24, 100, "xterm-256color")?
        .finish(Duration::from_secs(15))?;
    assert!(no_color.status.success());
    assert!(no_color.terminal_restored);
    let no_color_stderr = String::from_utf8_lossy(&no_color.stderr);
    assert!(no_color_stderr.contains('✓'));
    assert!(
        no_color_stderr.split(['\r', '\n']).any(|frame| {
            frame.rsplit_once(" · ").is_some_and(|(_, elapsed)| {
                elapsed
                    .split('s')
                    .next()
                    .is_some_and(|seconds| seconds.parse::<f64>().is_ok())
            })
        }),
        "settled PTY rows omitted final elapsed time: {no_color_stderr:?}"
    );
    for sequence in [
        b"\x1b[31m".as_slice(),
        b"\x1b[32m".as_slice(),
        b"\x1b[33m".as_slice(),
        b"\x1b[2m".as_slice(),
    ] {
        assert!(
            !no_color
                .stderr
                .windows(sequence.len())
                .any(|bytes| bytes == sequence)
        );
    }

    let mut no_color_flag_command = firestone(&home, &path);
    no_color_flag_command
        .args(["--no-color", "images", "pull"])
        .arg(&no_color_source)
        .env_remove("NO_COLOR");
    let no_color_flag = PtyCommand::spawn(no_color_flag_command, 24, 100, "xterm-256color")?
        .finish(Duration::from_secs(15))?;
    assert!(no_color_flag.status.success());
    assert!(no_color_flag.terminal_restored);
    for sequence in [
        b"\x1b[31m".as_slice(),
        b"\x1b[32m".as_slice(),
        b"\x1b[33m".as_slice(),
        b"\x1b[2m".as_slice(),
    ] {
        assert!(
            !no_color_flag
                .stderr
                .windows(sequence.len())
                .any(|bytes| bytes == sequence)
        );
    }

    let dumb_source = make_source("dumb")?;
    let mut dumb_command = firestone(&home, &path);
    dumb_command.args(["images", "pull"]).arg(&dumb_source);
    let dumb = PtyCommand::spawn(dumb_command, 24, 100, "dumb")?.finish(Duration::from_secs(15))?;
    assert!(dumb.status.success());
    assert!(dumb.terminal_restored);
    assert!(!dumb.stderr.contains(&0x1b));
    assert!(String::from_utf8_lossy(&dumb.stderr).contains("[image]"));

    let quiet_source = make_source("quiet")?;
    let mut quiet_command = firestone(&home, &path);
    quiet_command
        .args(["--quiet", "images", "pull"])
        .arg(&quiet_source);
    let quiet = PtyCommand::spawn(quiet_command, 24, 100, "xterm-256color")?
        .finish(Duration::from_secs(15))?;
    assert!(quiet.status.success());
    assert!(quiet.terminal_restored);
    assert!(quiet.stderr.is_empty());
    assert!(!quiet.stdout.is_empty());

    let plain_source = make_source("plain")?;
    let plain = firestone(&home, &path)
        .args(["images", "pull"])
        .arg(&plain_source)
        .output()?;
    assert!(plain.status.success());
    assert!(!plain.stderr.contains(&0x1b));
    assert!(!plain.stderr.contains(&b'\r'));
    assert!(String::from_utf8_lossy(&plain.stderr).contains("[image]"));

    let json_source = make_source("json")?;
    let json = firestone(&home, &path)
        .args(["--json", "images", "pull"])
        .arg(&json_source)
        .output()?;
    assert!(json.status.success());
    assert!(json.stderr.is_empty());
    assert!(!json.stdout.contains(&0x1b));
    assert!(!ndjson(&json)?.is_empty());
    Ok(())
}

#[test]
fn lifecycle_cli_smoke_without_kvm() -> TestResult {
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
        "stdout={} stderr={}",
        String::from_utf8_lossy(&stopped.stdout),
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

fn wait_for_state(
    path: &Path,
    timeout: Duration,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let state: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
        if predicate(&state) {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(format!("state predicate timed out: {state}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn require_success(output: &Output, action: &str) -> Result<(), Box<dyn Error>> {
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{action} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn event_index(records: &[serde_json::Value], kind: &str, id: &str, occurrence: usize) -> usize {
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["type"] == kind && record["id"] == id)
        .nth(occurrence)
        .map_or(usize::MAX, |(index, _)| index)
}

#[test]
fn m3_sidecars_cli_lifecycle_uses_exact_plans_and_recovers_degradation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(
        env!("CARGO_BIN_EXE_firestone"),
        fs::Permissions::from_mode(0o755),
    )?;
    let home = root.join("home");
    let fake = compile_fake_vmm(&root)?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
    for program in ["qemu-img", "passt"] {
        let path = bin.join(program);
        fs::copy(&fake, &path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;

    let source = root.join("m3-base.qcow2");
    fs::write(&source, b"QFI\xfbM3-CLI")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let first_host = root.join("first-host");
    let second_host = root.join("second-host");
    fs::create_dir(&first_host)?;
    fs::create_dir(&second_host)?;
    fs::set_permissions(&first_host, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&second_host, fs::Permissions::from_mode(0o700))?;
    let first_host = fs::canonicalize(first_host)?;
    let second_host = fs::canonicalize(second_host)?;
    let user_data = root.join("user-data.sh");
    fs::write(&user_data, b"#!/bin/sh\nprintf m3\n")?;
    fs::set_permissions(&user_data, fs::Permissions::from_mode(0o600))?;
    let network_config = root.join("network.yaml");
    fs::write(&network_config, b"version: 2\nethernets: {}\n")?;
    fs::set_permissions(&network_config, fs::Permissions::from_mode(0o600))?;
    let ssh_key = root.join("id.pub");
    fs::write(
        &ssh_key,
        b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEZzSm9dJz0ZVb1e8WnQwB6sNscXFGHvXcYzM8O5pBsx m3@test\n",
    )?;
    fs::set_permissions(&ssh_key, fs::Permissions::from_mode(0o600))?;
    let sidecar_record = root.join("sidecars.log");
    let request_record = root.join("requests.log");
    let body = root.join("body.json");
    let console = home.join("data/machines/m3/console.log");
    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        names: vec!["m3".to_owned()],
    };

    let mut create = firestone(&home, &path);
    create
        .arg("--json")
        .arg("create")
        .arg("m3")
        .arg(&source)
        .args(["-p", "18080:80"])
        .arg("--mount")
        .arg(format!("{}:/work", first_host.display()))
        .arg("--mount")
        .arg(format!("{}:/archive:ro", second_host.display()))
        .arg("--user-data")
        .arg(&user_data)
        .arg("--cloud-init-network-config")
        .arg(&network_config)
        .arg("--ssh-key")
        .arg(&ssh_key)
        .arg("--no-provisioning")
        .arg("--vmm-binary")
        .arg(&fake)
        .arg("--vmm-firmware")
        .arg(&firmware);
    for value in [
        "--record".to_owned(),
        request_record.to_string_lossy().into_owned(),
        "--body".to_owned(),
        body.to_string_lossy().into_owned(),
        "--behavior".to_owned(),
        "normal".to_owned(),
        "--console-log".to_owned(),
        console.to_string_lossy().into_owned(),
    ] {
        create.arg(format!("--vmm-arg={value}"));
    }
    let created = create.output()?;
    require_success(&created, "create")?;

    let data_bin = home.join("data/bin");
    fs::create_dir_all(&data_bin)?;
    fs::set_permissions(&data_bin, fs::Permissions::from_mode(0o700))?;
    let virtiofsd = data_bin.join("virtiofsd-v1.14.0");
    fs::copy(&fake, &virtiofsd)?;
    fs::set_permissions(&virtiofsd, fs::Permissions::from_mode(0o755))?;

    let mut start = firestone(&home, &path);
    let started = start
        .args(["--json", "start", "m3", "--no-wait", "--timeout", "15s"])
        .env("UNSAFE_TEST_ENV", "must-not-leak")
        .env("FIRESTONE_FAKE_SIDECAR_RECORD", &sidecar_record)
        .output()?;
    require_success(&started, "start")?;
    let start_records = ndjson(&started)?;
    let net = event_index(&start_records, "StepStart", "net", 0);
    let fs0 = event_index(&start_records, "StepStart", "fs", 0);
    let fs1 = event_index(&start_records, "StepStart", "fs", 1);
    let vmm = event_index(&start_records, "StepStart", "vmm", 0);
    assert!(net < fs0 && fs0 < fs1 && fs1 < vmm);
    let start_result = start_records
        .iter()
        .find(|record| record["type"] == "Result" && record["action"] == "start")
        .ok_or("start emitted no Result")?;
    assert_eq!(
        start_result["payload"]["forwards"],
        serde_json::json!(["18080:80"])
    );
    assert_eq!(
        start_result["payload"]["mounts"],
        serde_json::json!([
            format!("{} -> /work", first_host.display()),
            format!("{} -> /archive", second_host.display())
        ])
    );

    let state_path = home.join("data/machines/m3/state.json");
    let running = wait_for_state(&state_path, Duration::from_secs(2), |state| {
        state["status"] == "running"
            && state["sidecar_pids"]
                .as_object()
                .map_or(0, serde_json::Map::len)
                == 3
    })?;
    let original_instance = running["instance_id"]
        .as_str()
        .ok_or("missing instance id")?;
    let original_pids = running["sidecar_pids"]
        .as_object()
        .ok_or("missing sidecar pids")?
        .clone();

    let vmconfig: serde_json::Value = serde_json::from_slice(&fs::read(&body)?)?;
    assert_eq!(vmconfig["net"][0]["vhost_user"], true);
    assert_eq!(vmconfig["fs"].as_array().map(Vec::len), Some(2));
    assert_eq!(vmconfig["fs"][0]["tag"], "share0");
    assert_eq!(vmconfig["fs"][1]["tag"], "share1");

    let sidecars = fs::read_to_string(&sidecar_record)?;
    let lines = sidecars.lines().collect::<Vec<_>>();
    assert!(!sidecars.contains("UNSAFE_TEST_ENV"));
    let launch_passt = lines
        .iter()
        .position(|line| line.starts_with("launch passt "))
        .ok_or("passt was not launched")?;
    let launch_fs0 = lines
        .iter()
        .position(|line| line.starts_with("launch virtiofsd-0 "))
        .ok_or("virtiofsd-0 was not launched")?;
    let launch_fs1 = lines
        .iter()
        .position(|line| line.starts_with("launch virtiofsd-1 "))
        .ok_or("virtiofsd-1 was not launched")?;
    let first_connect = lines
        .iter()
        .position(|line| line.starts_with("connect "))
        .ok_or("VMM made no sidecar connection")?;
    assert!(launch_passt < launch_fs0 && launch_fs0 < launch_fs1 && launch_fs1 < first_connect);
    assert!(lines[launch_passt].contains("\"--repair-path\", \"none\"]"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("connect "))
            .count(),
        3
    );

    for source in ["passt", "virtiofsd-1"] {
        let logs = firestone(&home, &path)
            .args(["--json", "logs", "m3", "--source", source, "-n", "20"])
            .output()?;
        require_success(&logs, "logs")?;
        assert!(
            ndjson(&logs)?.iter().any(|record| {
                record["type"] == "Output"
                    && record["data"]
                        .as_str()
                        .is_some_and(|data| data.contains("fake") && data.contains("ready"))
            }),
            "missing {source} log output"
        );
    }

    let passt_pid = original_pids["passt"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("invalid passt pid")?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(passt_pid),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    let degraded = wait_for_state(&state_path, Duration::from_secs(2), |state| {
        state["degraded"].as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry == "passt exited (signal 9)")
        })
    })?;
    assert_eq!(degraded["status"], "running");
    assert!(degraded["sidecar_pids"].get("passt").is_none());
    let listed = firestone(&home, &path).args(["--json", "ls"]).output()?;
    require_success(&listed, "ls")?;
    let listed_records = ndjson(&listed)?;
    let expected_status = if cfg!(target_os = "linux") {
        "running!"
    } else {
        "running! (unsupervised)"
    };
    assert_eq!(listed_records[0]["payload"][0]["status"], expected_status);
    let shown = firestone(&home, &path)
        .args(["--json", "show", "m3"])
        .output()?;
    require_success(&shown, "show")?;
    assert!(
        ndjson(&shown)?[0]["payload"]["state"]["degraded"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry == "passt exited (signal 9)"))
    );

    let known_hosts = home.join("data/machines/m3/known_hosts");
    fs::write(&known_hosts, b"old-host-key\n")?;
    fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600))?;
    fs::write(
        &network_config,
        b"version: 2\nethernets:\n  eth0:\n    dhcp4: true\n",
    )?;
    let restarted = firestone(&home, &path)
        .args(["--json", "restart", "m3"])
        .env("FIRESTONE_FAKE_SIDECAR_RECORD", &sidecar_record)
        .output()?;
    let restart_context = format!(
        "restart; shim log:\n{}",
        fs::read_to_string(home.join("data/machines/m3/shim.log"))
            .unwrap_or_else(|error| format!("<unavailable: {error}>"))
    );
    require_success(&restarted, &restart_context)?;
    let restarted_state = wait_for_state(&state_path, Duration::from_secs(2), |state| {
        state["status"] == "running"
            && state["sidecar_pids"]
                .as_object()
                .map_or(0, serde_json::Map::len)
                == 3
    })?;
    assert!(
        restarted_state["degraded"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert_ne!(
        restarted_state["instance_id"].as_str(),
        Some(original_instance)
    );
    assert!(!known_hosts.exists());
    let restarted_pids = restarted_state["sidecar_pids"]
        .as_object()
        .ok_or("missing restarted pids")?
        .clone();
    for (sidecar, old_pid) in &original_pids {
        if let (Some(old_pid), Some(new_pid)) = (old_pid.as_u64(), restarted_pids[sidecar].as_u64())
        {
            assert_ne!(old_pid, new_pid, "{sidecar} was not replaced");
        }
    }

    let crashed_vmm = restarted_state["vmm_pid"]
        .as_u64()
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or("missing restarted VMM pid")?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(crashed_vmm),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    let failed_after_vmm_crash = wait_for_state(&state_path, Duration::from_secs(3), |state| {
        state["status"] == "failed"
            && state["sidecar_pids"]
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
    })?;
    assert_eq!(failed_after_vmm_crash["vmm_pid"], serde_json::Value::Null);
    let runtime_deadline = Instant::now() + Duration::from_secs(2);
    while home.join("run/m3").exists() && Instant::now() < runtime_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!home.join("run/m3").exists());
    for pid in restarted_pids
        .values()
        .filter_map(serde_json::Value::as_u64)
    {
        let pid = i32::try_from(pid)?;
        assert!(matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH)
        ));
    }

    let started_again = firestone(&home, &path)
        .args(["--json", "start", "m3", "--no-wait", "--timeout", "15s"])
        .env("FIRESTONE_FAKE_SIDECAR_RECORD", &sidecar_record)
        .output()?;
    require_success(&started_again, "start after VMM crash")?;
    let final_running = wait_for_state(&state_path, Duration::from_secs(2), |state| {
        state["status"] == "running"
            && state["sidecar_pids"]
                .as_object()
                .map_or(0, serde_json::Map::len)
                == 3
    })?;
    let final_sidecar_pids = final_running["sidecar_pids"]
        .as_object()
        .ok_or("missing final sidecar pids")?
        .clone();

    if cfg!(target_os = "linux") {
        let shim_pid = final_running["shim_pid"]
            .as_u64()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or("missing final shim pid")?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(shim_pid),
            nix::sys::signal::Signal::SIGKILL,
        )?;
        thread::sleep(Duration::from_millis(100));
    }
    let mut stop = firestone(&home, &path);
    stop.args(["--json", "stop", "m3", "--timeout", "2s"]);
    if cfg!(target_os = "linux") {
        stop.arg("--force");
    }
    let stopped = stop.output()?;
    require_success(&stopped, "stop")?;
    let stopped_state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path)?)?;
    assert_eq!(stopped_state["status"], "stopped");
    assert!(
        stopped_state["sidecar_pids"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
    assert!(!home.join("run/m3").exists());
    if cfg!(target_os = "linux") {
        for pid in final_sidecar_pids
            .values()
            .filter_map(serde_json::Value::as_u64)
        {
            let pid = i32::try_from(pid)?;
            assert!(matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
                Err(nix::errno::Errno::ESRCH)
            ));
        }
    }

    let removed = firestone(&home, &path)
        .args(["--json", "rm", "m3", "--force"])
        .output()?;
    require_success(&removed, "rm")?;
    assert!(!home.join("data/machines/m3").exists());
    Ok(())
}

fn fake_sidecar_pids(record: &str) -> Vec<i32> {
    record
        .lines()
        .filter_map(|line| line.split(" pid=").nth(1))
        .filter_map(|tail| tail.split_ascii_whitespace().next())
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

fn assert_processes_gone(record: &str) -> Result<(), Box<dyn Error>> {
    for pid in fake_sidecar_pids(record) {
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Err(nix::errno::Errno::ESRCH) => {}
            Ok(()) => return Err(format!("sidecar pid {pid} survived rollback").into()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[test]
fn m3_sidecar_partial_launch_and_sigint_roll_back_every_process() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(
        env!("CARGO_BIN_EXE_firestone"),
        fs::Permissions::from_mode(0o755),
    )?;
    let home = root.join("home");
    let fake = compile_fake_vmm(&root)?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
    for program in ["qemu-img", "passt"] {
        let target = bin.join(program);
        fs::copy(&fake, &target)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
    }
    let mut path_entries = vec![bin];
    if let Some(existing) = env::var_os("PATH") {
        path_entries.extend(env::split_paths(&existing));
    }
    let path = env::join_paths(path_entries)?;
    let source = root.join("base.qcow2");
    fs::write(&source, b"QFI\xfbM3-ROLLBACK")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.join("firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;
    let first_host = root.join("host-a");
    let second_host = root.join("host-b");
    fs::create_dir(&first_host)?;
    fs::create_dir(&second_host)?;
    fs::set_permissions(&first_host, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&second_host, fs::Permissions::from_mode(0o700))?;
    let first_host = fs::canonicalize(first_host)?;
    let second_host = fs::canonicalize(second_host)?;
    let names = vec![
        "rollback-passt".to_owned(),
        "rollback-fs0".to_owned(),
        "rollback-fs1".to_owned(),
        "rollback-timeout".to_owned(),
        "rollback-sigint".to_owned(),
    ];
    let _cleanup = MachineCleanup {
        home: home.clone(),
        path: path.clone(),
        names: names.clone(),
    };

    for (index, name) in names.iter().enumerate() {
        let request_record = root.join(format!("{name}-requests.log"));
        let body = root.join(format!("{name}-body.json"));
        let console = home.join("data/machines").join(name).join("console.log");
        let mut create = firestone(&home, &path);
        create
            .arg("--json")
            .arg("create")
            .arg(name)
            .arg(&source)
            .arg("--mount")
            .arg(format!("{}:/work", first_host.display()))
            .arg("--mount")
            .arg(format!("{}:/archive:ro", second_host.display()))
            .arg("--no-provisioning")
            .arg("--vmm-binary")
            .arg(&fake)
            .arg("--vmm-firmware")
            .arg(&firmware);
        for value in [
            "--record".to_owned(),
            request_record.to_string_lossy().into_owned(),
            "--body".to_owned(),
            body.to_string_lossy().into_owned(),
            "--behavior".to_owned(),
            "normal".to_owned(),
            "--console-log".to_owned(),
            console.to_string_lossy().into_owned(),
        ] {
            create.arg(format!("--vmm-arg={value}"));
        }
        let output = create.output()?;
        require_success(&output, "rollback create")?;
        if index == 0 {
            let data_bin = home.join("data/bin");
            fs::create_dir_all(&data_bin)?;
            fs::set_permissions(&data_bin, fs::Permissions::from_mode(0o700))?;
            let virtiofsd = data_bin.join("virtiofsd-v1.14.0");
            fs::copy(&fake, &virtiofsd)?;
            fs::set_permissions(virtiofsd, fs::Permissions::from_mode(0o755))?;
        }
    }

    for (name, boundary) in [
        ("rollback-passt", "passt"),
        ("rollback-fs0", "virtiofsd-0"),
        ("rollback-fs1", "virtiofsd-1"),
    ] {
        let record = root.join(format!("{name}-sidecars.log"));
        let failed = firestone(&home, &path)
            .args(["--json", "start", name, "--no-wait", "--timeout", "15s"])
            .env("FIRESTONE_FAKE_SIDECAR_RECORD", &record)
            .env("FIRESTONE_FAKE_SIDECAR_BAD_READY", boundary)
            .output()?;
        assert!(!failed.status.success(), "{boundary} unexpectedly started");
        let state_path = home.join("data/machines").join(name).join("state.json");
        let state: serde_json::Value = serde_json::from_slice(&fs::read(state_path)?)?;
        assert_eq!(state["status"], "failed");
        assert!(
            state["sidecar_pids"]
                .as_object()
                .is_some_and(serde_json::Map::is_empty),
            "{name} retained sidecars: {state}"
        );
        assert!(!home.join("run").join(name).exists());
        let recorded = fs::read_to_string(record)?;
        assert_processes_gone(&recorded)?;
    }

    let cancel_name = "rollback-sigint";
    let timeout_name = "rollback-timeout";
    let timeout_record = root.join("timeout-sidecars.log");
    let timed_out = firestone(&home, &path)
        .args([
            "--json",
            "start",
            timeout_name,
            "--no-wait",
            "--timeout",
            "20s",
        ])
        .env("FIRESTONE_FAKE_SIDECAR_RECORD", &timeout_record)
        .env("FIRESTONE_FAKE_SIDECAR_NEVER_READY", "passt")
        .output()?;
    assert_eq!(timed_out.status.code(), Some(6));
    let timeout_state_path = home
        .join("data/machines")
        .join(timeout_name)
        .join("state.json");
    let timeout_state = wait_for_state(&timeout_state_path, Duration::from_secs(5), |state| {
        state["status"] != "starting"
            && state["sidecar_pids"]
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
    })?;
    assert_eq!(timeout_state["status"], "failed");
    assert!(
        timeout_state["sidecar_pids"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
    assert!(!home.join("run").join(timeout_name).exists());
    let timeout_recorded = fs::read_to_string(&timeout_record).map_err(|error| {
        format!(
            "cannot read timeout sidecar record {}: {error}; start stdout: {}; stderr: {}",
            timeout_record.display(),
            String::from_utf8_lossy(&timed_out.stdout),
            String::from_utf8_lossy(&timed_out.stderr)
        )
    })?;
    assert!(timeout_recorded.contains("launch passt "));
    assert_processes_gone(&timeout_recorded)?;

    let cancel_record = root.join("cancel-sidecars.log");
    let mut command = firestone(&home, &path);
    command
        .args([
            "--json",
            "start",
            cancel_name,
            "--no-wait",
            "--timeout",
            "15s",
        ])
        .env("FIRESTONE_FAKE_SIDECAR_RECORD", &cancel_record)
        .env("FIRESTONE_FAKE_SIDECAR_NEVER_READY", "passt")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let launched_deadline = Instant::now() + Duration::from_secs(5);
    while !cancel_record.exists() || !fs::read_to_string(&cancel_record)?.contains("launch passt ")
    {
        if Instant::now() >= launched_deadline {
            child.kill()?;
            let _ = child.wait();
            return Err("cancelled start did not launch passt".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id())?),
        nix::sys::signal::Signal::SIGINT,
    )?;
    let status = match child.wait_timeout(Duration::from_secs(5))? {
        Some(status) => status,
        None => {
            child.kill()?;
            let _ = child.wait();
            return Err("start did not cancel within five seconds".into());
        }
    };
    assert!(!status.success());
    let cancel_state: serde_json::Value = serde_json::from_slice(&fs::read(
        home.join("data/machines")
            .join(cancel_name)
            .join("state.json"),
    )?)?;
    assert_eq!(cancel_state["status"], "created");
    assert!(
        cancel_state["sidecar_pids"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
    assert!(!home.join("run").join(cancel_name).exists());
    assert_processes_gone(&fs::read_to_string(cancel_record)?)?;
    Ok(())
}
