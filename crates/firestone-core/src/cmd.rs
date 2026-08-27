use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

use wait_timeout::ChildExt;

use crate::{ErrorKind, FirestoneError};

const DEFAULT_CAPTURE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct CmdArg {
    value: OsString,
    logged: OsString,
}

#[derive(Debug, Clone, Default)]
enum CmdStdin {
    #[default]
    Null,
    Inherit,
    Bytes(Vec<u8>),
}

/// The captured result of one external process.
#[derive(Debug)]
pub struct CmdOutput {
    program: OsString,
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

impl CmdOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.status.success()
    }

    #[must_use]
    pub const fn status(&self) -> ExitStatus {
        self.status
    }

    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[must_use]
    pub const fn stdout_truncated(&self) -> bool {
        self.stdout_truncated
    }

    #[must_use]
    pub const fn stderr_truncated(&self) -> bool {
        self.stderr_truncated
    }

    #[must_use]
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    #[must_use]
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    #[must_use]
    pub fn last_stderr_lines(&self) -> Vec<String> {
        last_lines(&self.stderr, 10)
    }

    fn failure(&self, kind: ErrorKind) -> FirestoneError {
        let status = status_label(self.status);
        let stderr = self.last_stderr_lines();
        let suffix = if stderr.is_empty() {
            "last stderr: <empty>".to_owned()
        } else {
            format!("last stderr:\n{}", stderr.join("\n"))
        };

        FirestoneError::new(
            kind,
            format!(
                "command `{}` exited with status {status}; {suffix}",
                self.program.to_string_lossy()
            ),
        )
    }
}

/// Constructs and runs a process without a shell.
///
/// Arguments are logged at debug level. Use [`Cmd::secret_arg`] for values that
/// must be passed on argv but must not be written to a log. Stdin bytes and
/// environment values are never logged.
#[derive(Debug, Clone)]
pub struct Cmd {
    program: OsString,
    args: Vec<CmdArg>,
    cwd: Option<PathBuf>,
    clear_env: bool,
    env: BTreeMap<OsString, OsString>,
    stdin: CmdStdin,
    stderr_log: Option<PathBuf>,
    error_kind: ErrorKind,
    timeout: Option<Duration>,
    capture_limit: usize,
}

impl Cmd {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            clear_env: false,
            env: BTreeMap::new(),
            stdin: CmdStdin::Null,
            stderr_log: None,
            error_kind: ErrorKind::Generic,
            timeout: None,
            capture_limit: DEFAULT_CAPTURE_LIMIT,
        }
    }

    #[must_use]
    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        let value = value.into();
        self.args.push(CmdArg {
            logged: value.clone(),
            value,
        });
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        for value in values {
            self = self.arg(value);
        }
        self
    }

    /// Adds an argument whose value is replaced by `<redacted>` in debug logs.
    #[must_use]
    pub fn secret_arg(mut self, value: impl Into<OsString>) -> Self {
        self.args.push(CmdArg {
            value: value.into(),
            logged: OsString::from("<redacted>"),
        });
        self
    }

    #[must_use]
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.cwd = Some(path.into());
        self
    }

    #[must_use]
    pub fn env_clear(mut self) -> Self {
        self.clear_env = true;
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn stdin_null(mut self) -> Self {
        self.stdin = CmdStdin::Null;
        self
    }

    #[must_use]
    pub fn stdin_inherit(mut self) -> Self {
        self.stdin = CmdStdin::Inherit;
        self
    }

    /// Supplies stdin without including its contents in process logs.
    #[must_use]
    pub fn stdin_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = CmdStdin::Bytes(bytes.into());
        self
    }

    /// Appends captured stderr to the supplied log after the process exits.
    #[must_use]
    pub fn stderr_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.stderr_log = Some(path.into());
        self
    }

    #[must_use]
    pub const fn error_kind(mut self, kind: ErrorKind) -> Self {
        self.error_kind = kind;
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub const fn capture_limit(mut self, bytes: usize) -> Self {
        self.capture_limit = bytes;
        self
    }

    /// Runs the process and returns captured output regardless of exit status.
    pub fn output(&self) -> Result<CmdOutput, FirestoneError> {
        let logged_argv = std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(|arg| arg.logged.as_os_str()))
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let env_keys = self
            .env
            .keys()
            .map(|key| key.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        tracing::debug!(
            argv = ?logged_argv,
            cwd = ?self.cwd,
            env_clear = self.clear_env,
            env_keys = ?env_keys,
            "starting external process"
        );

        let mut command = Command::new(&self.program);
        command.args(self.args.iter().map(|arg| &arg.value));
        command.stdout(Stdio::piped()).stderr(Stdio::piped());

        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(&self.env);

        match &self.stdin {
            CmdStdin::Null => {
                command.stdin(Stdio::null());
            }
            CmdStdin::Inherit => {
                command.stdin(Stdio::inherit());
            }
            CmdStdin::Bytes(_) => {
                command.stdin(Stdio::piped());
            }
        }

        let mut child = command.spawn().map_err(|source| {
            FirestoneError::new(
                self.error_kind,
                format!("cannot start command `{}`", self.program.to_string_lossy()),
            )
            .with_source(source)
        })?;

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FirestoneError::new(
                self.error_kind,
                format!(
                    "cannot capture stdout for command `{}`",
                    self.program.to_string_lossy()
                ),
            ));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FirestoneError::new(
                self.error_kind,
                format!(
                    "cannot capture stderr for command `{}`",
                    self.program.to_string_lossy()
                ),
            ));
        };

        let stderr_log = match &self.stderr_log {
            Some(path) => match open_log(path, self.error_kind) {
                Ok(file) => Some(file),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            },
            None => None,
        };
        let capture_limit = self.capture_limit;
        let stdout_reader =
            std::thread::spawn(move || capture_stream(stdout, capture_limit, None, false));
        let stderr_reader =
            std::thread::spawn(move || capture_stream(stderr, capture_limit, stderr_log, true));

        let stdin_writer = if let CmdStdin::Bytes(bytes) = &self.stdin {
            let Some(mut stdin) = child.stdin.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FirestoneError::new(
                    self.error_kind,
                    format!(
                        "cannot open stdin for command `{}`",
                        self.program.to_string_lossy()
                    ),
                ));
            };
            let bytes = bytes.clone();
            Some(std::thread::spawn(move || stdin.write_all(&bytes)))
        } else {
            None
        };

        let (status_result, timed_out) = match self.timeout {
            Some(timeout) => match child.wait_timeout(timeout) {
                Ok(Some(status)) => (Ok(status), false),
                Ok(None) => {
                    let kill_result = child.kill();
                    let wait_result = child.wait();
                    match (kill_result, wait_result) {
                        (_, Ok(status)) => (Ok(status), true),
                        (Err(source), _) | (_, Err(source)) => (Err(source), true),
                    }
                }
                Err(source) => (Err(source), false),
            },
            None => (child.wait(), false),
        };
        if let Some(writer) = stdin_writer {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(source)) => {
                    return Err(FirestoneError::new(
                        self.error_kind,
                        format!(
                            "cannot write stdin for command `{}`",
                            self.program.to_string_lossy()
                        ),
                    )
                    .with_source(source));
                }
                Err(_) => {
                    return Err(FirestoneError::new(
                        self.error_kind,
                        format!(
                            "stdin writer panicked for command `{}`",
                            self.program.to_string_lossy()
                        ),
                    ));
                }
            }
        }

        let stdout = join_capture(stdout_reader, &self.program, "stdout", self.error_kind)?;
        let stderr = join_capture(stderr_reader, &self.program, "stderr", self.error_kind)?;
        let status = status_result.map_err(|source| {
            FirestoneError::new(
                self.error_kind,
                format!(
                    "cannot wait for command `{}`",
                    self.program.to_string_lossy()
                ),
            )
            .with_source(source)
        })?;
        if timed_out {
            let detail = last_lines(&stderr.bytes, 10);
            let suffix = if detail.is_empty() {
                "last stderr: <empty>".to_owned()
            } else {
                format!("last stderr:\n{}", detail.join("\n"))
            };
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "command `{}` timed out after {} ms; {suffix}",
                    self.program.to_string_lossy(),
                    self.timeout.map_or(0, |timeout| timeout.as_millis())
                ),
            ));
        }

        Ok(CmdOutput {
            program: self.program.clone(),
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        })
    }

    /// Runs the process and maps a non-zero exit to a contextual error.
    pub fn run(&self) -> Result<CmdOutput, FirestoneError> {
        let output = self.output()?;
        if output.success() {
            Ok(output)
        } else {
            Err(output.failure(self.error_kind))
        }
    }
}

#[derive(Debug)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

fn open_log(path: &Path, kind: ErrorKind) -> Result<File, FirestoneError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| {
            FirestoneError::new(
                kind,
                format!("cannot open command log `{}`", path.display()),
            )
            .with_source(source)
        })
}

fn capture_stream(
    mut reader: impl Read,
    limit: usize,
    mut log: Option<File>,
    retain_tail: bool,
) -> Result<Captured, std::io::Error> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if let Some(file) = &mut log {
            file.write_all(&buffer[..read])?;
        }
        if retain_tail {
            if read >= limit {
                bytes.clear();
                bytes.extend_from_slice(&buffer[read - limit..read]);
                truncated = true;
            } else {
                let overflow = bytes.len().saturating_add(read).saturating_sub(limit);
                if overflow > 0 {
                    bytes.drain(..overflow);
                    truncated = true;
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
        } else {
            let remaining = limit.saturating_sub(bytes.len());
            let retained = remaining.min(read);
            bytes.extend_from_slice(&buffer[..retained]);
            truncated |= retained < read;
        }
    }
    if let Some(file) = &mut log {
        file.flush()?;
    }
    Ok(Captured { bytes, truncated })
}

fn join_capture(
    reader: std::thread::JoinHandle<Result<Captured, std::io::Error>>,
    program: &std::ffi::OsStr,
    stream: &str,
    kind: ErrorKind,
) -> Result<Captured, FirestoneError> {
    match reader.join() {
        Ok(Ok(captured)) => Ok(captured),
        Ok(Err(source)) => Err(FirestoneError::new(
            kind,
            format!(
                "cannot capture {stream} for command `{}`",
                program.to_string_lossy()
            ),
        )
        .with_source(source)),
        Err(_) => Err(FirestoneError::new(
            kind,
            format!(
                "{stream} capture panicked for command `{}`",
                program.to_string_lossy()
            ),
        )),
    }
}

fn last_lines(bytes: &[u8], count: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text
        .lines()
        .rev()
        .take(count)
        .map(|line| {
            let mut characters = line.chars();
            let mut bounded = characters.by_ref().take(4096).collect::<String>();
            if characters.next().is_some() {
                bounded.push_str("...[truncated]");
            }
            bounded
        })
        .collect::<Vec<_>>();
    lines.reverse();
    lines
}

fn status_label(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::{Cmd, last_lines};

    fn executable(
        dir: &TempDir,
        name: &str,
        body: &str,
    ) -> Result<std::path::PathBuf, std::io::Error> {
        let path = dir.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    #[test]
    fn output_controlled_context_captures_streams_and_log() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new()?;
        let script = executable(
            &dir,
            "probe",
            "printf '%s\\n' \"$DOCTOR_VALUE\"; pwd; cat; printf 'captured\\n' >&2",
        )?;
        let log = dir.path().join("probe.log");

        let output = Cmd::new(script)
            .cwd(dir.path())
            .env_clear()
            .env("DOCTOR_VALUE", "set")
            .stdin_bytes(b"stdin\n".to_vec())
            .stderr_log(&log)
            .run()?;

        let stdout = output.stdout_lossy();
        assert!(stdout.starts_with("set\n"));
        assert!(stdout.contains(&dir.path().display().to_string()));
        assert!(stdout.ends_with("stdin\n"));
        assert_eq!(fs::read_to_string(log)?, "captured\n");
        Ok(())
    }

    #[test]
    fn run_nonzero_reports_program_status_and_last_ten_lines()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(
            &dir,
            "failure",
            "i=1; while [ \"$i\" -le 12 ]; do printf 'line-%s\\n' \"$i\" >&2; i=$((i + 1)); done; exit 23",
        )?;

        let error = match Cmd::new(&script).run() {
            Err(error) => error,
            Ok(_) => return Err(std::io::Error::other("command should fail").into()),
        };
        let message = error.message();

        assert!(message.contains(&script.display().to_string()));
        assert!(message.contains("status 23"));
        assert!(!message.contains("line-2\n"));
        assert!(message.contains("line-3"));
        assert!(message.contains("line-12"));
        Ok(())
    }

    #[test]
    fn stderr_single_long_line_is_bounded() {
        let lines = last_lines(&vec![b'x'; 10_000], 10);

        assert_eq!(lines.len(), 1);
        assert!(lines[0].len() < 4200);
        assert!(lines[0].ends_with("...[truncated]"));
    }

    #[test]
    fn output_capture_limit_drains_and_marks_truncated() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(
            &dir,
            "noisy",
            "i=0; while [ \"$i\" -lt 1000 ]; do printf 0123456789; i=$((i + 1)); done",
        )?;

        let output = Cmd::new(script).capture_limit(128).run()?;

        assert_eq!(output.stdout().len(), 128);
        assert!(output.stdout_truncated());
        assert!(!output.stderr_truncated());
        Ok(())
    }

    #[test]
    fn output_timeout_kills_and_reaps_process() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "hang", "while :; do :; done")?;
        let started = Instant::now();

        let error = match Cmd::new(script).timeout(Duration::from_millis(100)).run() {
            Err(error) => error,
            Ok(_) => return Err(std::io::Error::other("command should time out").into()),
        };

        assert_eq!(error.kind(), crate::ErrorKind::Timeout);
        assert!(error.message().contains("timed out after 100 ms"));
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }
}
