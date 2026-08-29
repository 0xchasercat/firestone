use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getpgid, getuid},
};
use wait_timeout::ChildExt;

use crate::{ErrorKind, FirestoneError};

const DEFAULT_CAPTURE_LIMIT: usize = 1024 * 1024;
const EXECUTABLE_BUSY_RETRY_DELAY: Duration = Duration::from_millis(5);
const EXECUTABLE_BUSY_MAX_RETRIES: usize = 20;
const PROCESS_LOG_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Clone)]
struct CmdArg {
    value: OsString,
    logged: OsString,
}

impl std::fmt::Debug for CmdArg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("CmdArg").field(&self.logged).finish()
    }
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
        .with_hint("inspect the reported stderr, correct the command failure, and retry")
    }
}

/// Constructs and runs a process without a shell.
///
/// Arguments are logged at debug level. Use [`Cmd::secret_arg`] for values that
/// must be passed on argv but must not be written to a log. Stdin bytes and
/// environment values are never logged.
#[derive(Clone)]
pub struct Cmd {
    program: OsString,
    args: Vec<CmdArg>,
    cwd: Option<PathBuf>,
    clear_env: bool,
    env: BTreeMap<OsString, OsString>,
    stdin: CmdStdin,
    stderr_log: Option<PathBuf>,
    stdout_append: Option<PathBuf>,
    stderr_append: Option<PathBuf>,
    error_kind: ErrorKind,
    timeout: Option<Duration>,
    capture_limit: usize,
    interactive_stdout_to_stderr: bool,
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stdin = match &self.stdin {
            CmdStdin::Null => "null",
            CmdStdin::Inherit => "inherit",
            CmdStdin::Bytes(_) => "bytes <redacted>",
        };
        formatter
            .debug_struct("Cmd")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("clear_env", &self.clear_env)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("stdin", &stdin)
            .field("stderr_log", &self.stderr_log)
            .field("stdout_append", &self.stdout_append)
            .field("stderr_append", &self.stderr_append)
            .field("error_kind", &self.error_kind)
            .field("timeout", &self.timeout)
            .field("capture_limit", &self.capture_limit)
            .field(
                "interactive_stdout_to_stderr",
                &self.interactive_stdout_to_stderr,
            )
            .finish()
    }
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
            stdout_append: None,
            stderr_append: None,
            error_kind: ErrorKind::Generic,
            timeout: None,
            capture_limit: DEFAULT_CAPTURE_LIMIT,
            interactive_stdout_to_stderr: false,
        }
    }
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn arguments(&self) -> impl ExactSizeIterator<Item = &OsStr> {
        self.args.iter().map(|argument| argument.value.as_os_str())
    }

    #[must_use]
    pub const fn clears_environment(&self) -> bool {
        self.clear_env
    }

    #[must_use]
    pub const fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }

    #[must_use]
    pub fn working_directory(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    #[must_use]
    pub fn stdout_append_path(&self) -> Option<&Path> {
        self.stdout_append.as_deref()
    }

    #[must_use]
    pub fn stderr_append_path(&self) -> Option<&Path> {
        self.stderr_append.as_deref()
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
    /// Clears the inherited environment, then copies only the process context
    /// allowed for Firestone children.
    #[must_use]
    pub fn reduced_environment(mut self) -> Self {
        self = self.env_clear();
        for key in [
            "PATH",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
        ] {
            if let Some(value) = env::var_os(key).filter(|value| !value.is_empty()) {
                self = self.env(key, value);
            }
        }
        for (key, value) in env::vars_os() {
            if key.to_string_lossy().starts_with("FIRESTONE_") {
                self = self.env(key, value);
            }
        }
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

    /// Routes an interactive child's stdout to the caller's stderr.
    #[must_use]
    pub const fn interactive_stdout_to_stderr(mut self) -> Self {
        self.interactive_stdout_to_stderr = true;
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

    /// Routes a long-running child's stdout directly to an append-only log.
    #[must_use]
    pub fn stdout_append(mut self, path: impl Into<PathBuf>) -> Self {
        self.stdout_append = Some(path.into());
        self
    }

    /// Routes a long-running child's stderr directly to an append-only log.
    #[must_use]
    pub fn stderr_append(mut self, path: impl Into<PathBuf>) -> Self {
        self.stderr_append = Some(path.into());
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
        command.process_group(0);
        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);

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

        let mut child = spawn_with_busy_retry(&mut command, deadline)
            .map_err(|source| command_start_error(self.error_kind, &self.program, source))?;
        let process_group = child.id();
        let Some(stdout) = child.stdout.take() else {
            let _ = kill_process_group(process_group);
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
            let _ = kill_process_group(process_group);
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
                    let _ = kill_process_group(process_group);
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
                let _ = kill_process_group(process_group);
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

        let mut timed_out = false;
        let status_result = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match child.wait_timeout(remaining) {
                    Ok(Some(status)) => Ok(status),
                    Ok(None) => {
                        timed_out = true;
                        let kill_result = kill_process_group(process_group);
                        let wait_result = child.wait();
                        match (kill_result, wait_result) {
                            (_, Ok(status)) => Ok(status),
                            (Err(source), _) | (_, Err(source)) => Err(source),
                        }
                    }
                    Err(source) => Err(source),
                }
            }
            None => child.wait(),
        };
        let stdin_error = if let Some(writer) = stdin_writer {
            match join_before_deadline(writer, deadline, process_group, &mut timed_out) {
                Ok(Ok(())) => None,
                Ok(Err(_)) if timed_out => None,
                Ok(Err(source)) => Some(
                    FirestoneError::new(
                        self.error_kind,
                        format!(
                            "cannot write stdin for command `{}`",
                            self.program.to_string_lossy()
                        ),
                    )
                    .with_hint("verify the command input and retry")
                    .with_source(source),
                ),
                Err(_) => Some(
                    FirestoneError::new(
                        self.error_kind,
                        format!(
                            "stdin writer panicked for command `{}`",
                            self.program.to_string_lossy()
                        ),
                    )
                    .with_hint("retry the command; preserve the error if it recurs"),
                ),
            }
        } else {
            None
        };

        let stdout = join_capture(
            stdout_reader,
            &self.program,
            "stdout",
            self.error_kind,
            deadline,
            process_group,
            &mut timed_out,
        )?;
        let stderr = join_capture(
            stderr_reader,
            &self.program,
            "stderr",
            self.error_kind,
            deadline,
            process_group,
            &mut timed_out,
        )?;
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
            )
            .with_hint("inspect the reported process output and retry the command"));
        }

        let output = CmdOutput {
            program: self.program.clone(),
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        };
        if output.success() {
            if let Some(error) = stdin_error {
                return Err(error);
            }
        }
        Ok(output)
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

    /// Starts a long-running child in a new process group.
    ///
    /// The caller owns supervision and must eventually reap the returned child.
    /// Stdin defaults to `/dev/null`; stdout and stderr must be explicitly routed
    /// with [`Self::stdout_append`] and [`Self::stderr_append`].
    pub fn spawn_process_group(&self) -> Result<ManagedProcess, FirestoneError> {
        self.spawn_long_running(true)
    }

    /// Starts a child which will call `setsid(2)` immediately after exec.
    ///
    /// Unlike [`Self::spawn_process_group`], this does not make the child a
    /// process-group leader before exec, because a group leader cannot create a
    /// new session. This primitive is reserved for the Firestone shim entrypoint.
    pub fn spawn_session_candidate(&self) -> Result<ManagedProcess, FirestoneError> {
        self.spawn_long_running(false)
    }

    fn spawn_long_running(&self, process_group: bool) -> Result<ManagedProcess, FirestoneError> {
        if matches!(self.stdin, CmdStdin::Bytes(_)) {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "long-running commands cannot use buffered stdin",
            )
            .with_hint("use stdin_null or stdin_inherit"));
        }
        if self.stderr_log.is_some() {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "long-running commands cannot use captured stderr logging",
            )
            .with_hint("use stderr_append for supervised processes"));
        }

        let stdout_path = self.stdout_append.as_ref().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                "long-running command stdout has no append log",
            )
            .with_hint("route stdout with stdout_append")
        })?;
        let stderr_path = self.stderr_append.as_ref().ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                "long-running command stderr has no append log",
            )
            .with_hint("route stderr with stderr_append")
        })?;
        let stdout = open_log(stdout_path, self.error_kind)?;
        let stderr = open_log(stderr_path, self.error_kind)?;

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
            process_group,
            "starting supervised external process"
        );

        let mut command = Command::new(&self.program);
        command.args(self.args.iter().map(|arg| &arg.value));
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        if process_group {
            command.process_group(0);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(&self.env);
        match self.stdin {
            CmdStdin::Null => {
                command.stdin(Stdio::null());
            }
            CmdStdin::Inherit => {
                command.stdin(Stdio::inherit());
            }
            CmdStdin::Bytes(_) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "long-running commands cannot use buffered stdin",
                ));
            }
        }

        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        let child = spawn_with_busy_retry(&mut command, deadline)
            .map_err(|source| command_start_error(self.error_kind, &self.program, source))?;
        let process_group = process_group.then_some(child.id());
        Ok(ManagedProcess {
            child,
            process_group,
            reaped: false,
            diagnostic: ProcessDiagnostic {
                program: self.program.clone(),
                kind: self.error_kind,
                log: Some(stderr_path.clone()),
            },
        })
    }
    /// Starts a terminal-facing process in its own process group.
    ///
    /// The caller owns signal forwarding and reaping. Stdin follows the
    /// configured null/inherit setting and stdout/stderr are inherited.
    pub fn spawn_interactive_process_group(&self) -> Result<ManagedProcess, FirestoneError> {
        if matches!(self.stdin, CmdStdin::Bytes(_))
            || self.stderr_log.is_some()
            || self.stdout_append.is_some()
            || self.stderr_append.is_some()
        {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "interactive process groups cannot use buffered input or log redirection",
            ));
        }

        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        let mut command = Command::new(&self.program);
        command.args(self.args.iter().map(|arg| &arg.value));
        command.stderr(Stdio::inherit()).process_group(0);
        if self.interactive_stdout_to_stderr {
            let stderr = OpenOptions::new()
                .write(true)
                .open("/dev/stderr")
                .map_err(|source| {
                    FirestoneError::new(
                        self.error_kind,
                        "cannot route interactive command stdout to stderr",
                    )
                    .with_source(source)
                })?;
            command.stdout(Stdio::from(stderr));
        } else {
            command.stdout(Stdio::inherit());
        }
        match self.stdin {
            CmdStdin::Null => {
                command.stdin(Stdio::null());
            }
            CmdStdin::Inherit => {
                command.stdin(Stdio::inherit());
            }
            CmdStdin::Bytes(_) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "interactive process groups cannot use buffered input",
                ));
            }
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(&self.env);

        let child = spawn_with_busy_retry(&mut command, deadline)
            .map_err(|source| command_start_error(self.error_kind, &self.program, source))?;
        let process_group = Some(child.id());
        Ok(ManagedProcess {
            child,
            process_group,
            reaped: false,
            diagnostic: ProcessDiagnostic {
                program: self.program.clone(),
                kind: self.error_kind,
                log: None,
            },
        })
    }

    /// Runs a terminal-facing process with inherited stdout and stderr and
    /// returns its exact Unix exit status.
    ///
    /// Stdin follows the configured null/inherit setting; byte-buffered stdin
    /// and stderr log capture are rejected.
    pub fn status_interactive(&self) -> Result<ExitStatus, FirestoneError> {
        if matches!(self.stdin, CmdStdin::Bytes(_)) {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "interactive commands cannot use buffered stdin",
            )
            .with_hint("use stdin_inherit for an interactive command"));
        }
        if self.stderr_log.is_some() {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "interactive commands cannot capture a stderr log",
            )
            .with_hint("remove stderr_log or use output/run instead"));
        }

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
            "starting interactive external process"
        );

        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        let mut command = Command::new(&self.program);
        command.args(self.args.iter().map(|arg| &arg.value));
        command.stderr(Stdio::inherit());
        if self.interactive_stdout_to_stderr {
            let stderr = OpenOptions::new()
                .write(true)
                .open("/dev/stderr")
                .map_err(|source| {
                    FirestoneError::new(
                        self.error_kind,
                        "cannot route interactive command stdout to stderr",
                    )
                    .with_hint("check that the process has an open stderr stream")
                    .with_source(source)
                })?;
            command.stdout(Stdio::from(stderr));
        } else {
            command.stdout(Stdio::inherit());
        }
        match self.stdin {
            CmdStdin::Null => {
                command.stdin(Stdio::null());
            }
            CmdStdin::Inherit => {
                command.stdin(Stdio::inherit());
            }
            CmdStdin::Bytes(_) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "interactive commands cannot use buffered stdin",
                ));
            }
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(&self.env);

        let mut child = spawn_with_busy_retry(&mut command, deadline)
            .map_err(|source| command_start_error(self.error_kind, &self.program, source))?;
        let (status, timed_out) = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match child.wait_timeout(remaining) {
                    Ok(Some(status)) => (status, false),
                    Ok(None) => {
                        child.kill().map_err(|source| {
                            FirestoneError::new(
                                self.error_kind,
                                format!(
                                    "cannot stop timed-out command `{}`",
                                    self.program.to_string_lossy()
                                ),
                            )
                            .with_source(source)
                        })?;
                        let status = child.wait().map_err(|source| {
                            FirestoneError::new(
                                self.error_kind,
                                format!(
                                    "cannot wait for timed-out command `{}`",
                                    self.program.to_string_lossy()
                                ),
                            )
                            .with_source(source)
                        })?;
                        (status, true)
                    }
                    Err(source) => {
                        return Err(FirestoneError::new(
                            self.error_kind,
                            format!(
                                "cannot wait for command `{}`",
                                self.program.to_string_lossy()
                            ),
                        )
                        .with_source(source));
                    }
                }
            }
            None => (
                child.wait().map_err(|source| {
                    FirestoneError::new(
                        self.error_kind,
                        format!(
                            "cannot wait for command `{}`",
                            self.program.to_string_lossy()
                        ),
                    )
                    .with_source(source)
                })?,
                false,
            ),
        };
        if timed_out {
            return Err(FirestoneError::new(
                ErrorKind::Timeout,
                format!(
                    "command {} timed out after {} ms",
                    self.program.to_string_lossy(),
                    self.timeout.map_or(0, |timeout| timeout.as_millis())
                ),
            )
            .with_hint("inspect the reported process output and retry the command"));
        }
        Ok(status)
    }

    /// Runs an interactive command and maps a non-zero status to an error.
    pub fn run_interactive(&self) -> Result<(), FirestoneError> {
        let status = self.status_interactive()?;
        if status.success() {
            return Ok(());
        }
        Err(ProcessDiagnostic {
            program: self.program.clone(),
            kind: self.error_kind,
            log: None,
        }
        .exit_error(
            status,
            "interactive command failed",
            "inspect the terminal output, correct the command failure, and retry",
        ))
    }

    /// Replaces the current process with this command.
    ///
    /// Successful execution never returns, preserving the child's exit and
    /// signal status exactly.
    pub fn exec(&self) -> Result<std::convert::Infallible, FirestoneError> {
        if matches!(self.stdin, CmdStdin::Bytes(_))
            || self.stderr_log.is_some()
            || self.stdout_append.is_some()
            || self.stderr_append.is_some()
            || self.timeout.is_some()
        {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "exec commands cannot use buffered input, log redirection, or a timeout",
            ));
        }

        let logged_argv = std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(|arg| arg.logged.as_os_str()))
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        tracing::debug!(argv = ?logged_argv, cwd = ?self.cwd, "execing external process");

        let mut command = Command::new(&self.program);
        command.args(self.args.iter().map(|arg| &arg.value));
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        match self.stdin {
            CmdStdin::Null => {
                command.stdin(Stdio::null());
            }
            CmdStdin::Inherit => {
                command.stdin(Stdio::inherit());
            }
            CmdStdin::Bytes(_) => {
                return Err(FirestoneError::new(
                    ErrorKind::Generic,
                    "exec commands cannot use buffered input",
                ));
            }
        }
        if self.interactive_stdout_to_stderr {
            let stderr = OpenOptions::new()
                .write(true)
                .open("/dev/stderr")
                .map_err(|source| {
                    FirestoneError::new(
                        self.error_kind,
                        "cannot route exec command stdout to stderr",
                    )
                    .with_source(source)
                })?;
            command.stdout(Stdio::from(stderr));
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(&self.env);

        let source = command.exec();
        Err(FirestoneError::new(
            self.error_kind,
            format!("cannot exec command {}", self.program.to_string_lossy()),
        )
        .with_hint("check that the program exists and is executable")
        .with_source(source))
    }
}

/// Signals accepted by the shared supervised-process primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
    Kill,
}

impl ProcessSignal {
    const fn as_nix(self) -> Signal {
        match self {
            Self::Hangup => Signal::SIGHUP,
            Self::Interrupt => Signal::SIGINT,
            Self::Quit => Signal::SIGQUIT,
            Self::Terminate => Signal::SIGTERM,
            Self::Kill => Signal::SIGKILL,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProcessDiagnostic {
    program: OsString,
    kind: ErrorKind,
    log: Option<PathBuf>,
}

impl ProcessDiagnostic {
    pub(crate) fn exit_error(
        &self,
        status: ExitStatus,
        context: &str,
        hint: impl Into<String>,
    ) -> FirestoneError {
        let status = status_label(status);
        let suffix = match self.log.as_deref() {
            Some(path) => match read_process_log_tail(path) {
                Ok(lines) if lines.is_empty() => "last 10 log lines: <empty>".to_owned(),
                Ok(lines) => format!("last 10 log lines:\n{}", lines.join("\n")),
                Err(source) => format!("last log lines unavailable: {source}"),
            },
            None => "last log lines unavailable: stderr was inherited".to_owned(),
        };
        FirestoneError::new(
            self.kind,
            format!(
                "{context}: command `{}` exited with status {status}; {suffix}",
                self.program.to_string_lossy()
            ),
        )
        .with_hint(hint)
    }
}

/// A long-running child and the process group created for it.
///
/// Dropping this value deliberately does not kill the child. Supervisors must
/// make their teardown policy explicit and reap every direct child.
#[derive(Debug)]
pub struct ManagedProcess {
    child: Child,
    process_group: Option<u32>,
    reaped: bool,
    diagnostic: ProcessDiagnostic,
}

impl ManagedProcess {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    #[must_use]
    pub const fn process_group(&self) -> Option<u32> {
        self.process_group
    }

    #[must_use]
    pub(crate) fn diagnostic(&self) -> ProcessDiagnostic {
        self.diagnostic.clone()
    }

    pub fn exit_error(
        &self,
        status: ExitStatus,
        context: &str,
        hint: impl Into<String>,
    ) -> FirestoneError {
        self.diagnostic.exit_error(status, context, hint)
    }

    /// Confirms that a session-candidate child made its pid the process-group id.
    pub fn confirm_session(&mut self) -> Result<(), FirestoneError> {
        let pid = process_pid(self.id())?;
        let actual = getpgid(Some(pid)).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot inspect process group for pid {}", self.id()),
            )
            .with_source(std::io::Error::from_raw_os_error(source as i32))
        })?;
        if actual != pid {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "process {} did not create its required session and process group",
                    self.id()
                ),
            ));
        }
        self.process_group = Some(self.id());
        Ok(())
    }

    /// Observes child exit without reaping, keeping its pid/process-group id pinned.
    pub fn observe_exit(&self) -> Result<bool, FirestoneError> {
        if self.reaped {
            return Ok(true);
        }
        let raw = i32::try_from(self.id()).map_err(|_| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("process id {} does not fit pid_t", self.id()),
            )
        })?;
        let pid = rustix::process::Pid::from_raw(raw).ok_or_else(|| {
            FirestoneError::new(ErrorKind::Generic, "child process id cannot be zero")
        })?;
        let options = rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT;
        let status = rustix::process::waitid(rustix::process::WaitId::Pid(pid), options).map_err(
            |source| {
                FirestoneError::new(
                    ErrorKind::Generic,
                    format!("cannot observe child process {}", self.id()),
                )
                .with_source(std::io::Error::from_raw_os_error(source.raw_os_error()))
            },
        )?;
        Ok(status.is_some_and(|status| status.exited() || status.killed() || status.dumped()))
    }

    #[must_use]
    pub const fn is_reaped(&self) -> bool {
        self.reaped
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, FirestoneError> {
        let status = self.child.try_wait().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot inspect child process {}", self.id()),
            )
            .with_source(source)
        })?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    pub fn wait(&mut self) -> Result<ExitStatus, FirestoneError> {
        let status = self.child.wait().map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot reap child process {}", self.id()),
            )
            .with_source(source)
        })?;
        self.reaped = true;
        Ok(status)
    }

    pub fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ExitStatus>, FirestoneError> {
        let status = self.child.wait_timeout(timeout).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot wait for child process {}", self.id()),
            )
            .with_source(source)
        })?;
        self.reaped |= status.is_some();
        Ok(status)
    }

    pub fn signal_process(&self, signal: ProcessSignal) -> Result<(), FirestoneError> {
        if self.reaped {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!("refusing to signal already-reaped pid {}", self.id()),
            ));
        }
        let pid = process_pid(self.id())?;
        match kill(pid, signal.as_nix()) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(source) => Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot signal child process {}", self.id()),
            )
            .with_source(std::io::Error::from_raw_os_error(source as i32))),
        }
    }

    pub fn signal_group(&self, signal: ProcessSignal) -> Result<(), FirestoneError> {
        if self.reaped {
            return Err(FirestoneError::new(
                ErrorKind::Conflict,
                format!(
                    "refusing to signal group of already-reaped pid {}",
                    self.id()
                ),
            ));
        }
        let group = self.process_group.ok_or_else(|| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("process {} has no verified process group", self.id()),
            )
        })?;
        let group = process_pid(group)?;
        match killpg(group, signal.as_nix()) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(source) => Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot signal process group {}", group.as_raw()),
            )
            .with_source(std::io::Error::from_raw_os_error(source as i32))),
        }
    }
}

fn process_pid(pid: u32) -> Result<Pid, FirestoneError> {
    i32::try_from(pid).map(Pid::from_raw).map_err(|_| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("process id {pid} does not fit pid_t"),
        )
    })
}

#[derive(Debug)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

fn command_start_error(kind: ErrorKind, program: &OsStr, source: std::io::Error) -> FirestoneError {
    FirestoneError::new(
        kind,
        format!(
            "cannot start command `{}`: {source}",
            program.to_string_lossy()
        ),
    )
    .with_hint("check that the program exists and is executable")
    .with_source(source)
}

fn spawn_with_busy_retry(
    command: &mut Command,
    deadline: Option<Instant>,
) -> Result<Child, std::io::Error> {
    let mut retries = 0_usize;
    let mut last_busy = None;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(last_busy
                .take()
                .unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut)));
        }
        match command.spawn() {
            Err(source)
                if source.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && retries < EXECUTABLE_BUSY_MAX_RETRIES =>
            {
                if let Some(deadline) = deadline {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining <= EXECUTABLE_BUSY_RETRY_DELAY {
                        if !remaining.is_zero() {
                            std::thread::sleep(remaining);
                        }
                        return Err(source);
                    }
                }
                std::thread::sleep(EXECUTABLE_BUSY_RETRY_DELAY);
                retries += 1;
                last_busy = Some(source);
            }
            result => return result,
        }
    }
}
fn read_process_log_tail(path: &Path) -> Result<Vec<String>, std::io::Error> {
    let flags = nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    let metadata = file.metadata()?;
    let mode = metadata.mode() & 0o7777;
    let uid = getuid().as_raw();
    if !metadata.is_file() || metadata.uid() != uid || mode != 0o600 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "process log {} must be a mode-0600 regular file owned by uid {uid}",
                path.display()
            ),
        ));
    }
    let length = metadata.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(PROCESS_LOG_TAIL_BYTES),
    ))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(PROCESS_LOG_TAIL_BYTES)).unwrap_or(64 * 1024),
    );
    file.take(PROCESS_LOG_TAIL_BYTES).read_to_end(&mut bytes)?;
    Ok(last_lines(&bytes, 10))
}

fn open_log(path: &Path, kind: ErrorKind) -> Result<File, FirestoneError> {
    let flags = nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK;
    let created = match OpenOptions::new()
        .read(true)
        .append(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => (file, true),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            let file = OpenOptions::new()
                .read(true)
                .append(true)
                .custom_flags(flags)
                .open(path)
                .map_err(|source| {
                    FirestoneError::new(
                        kind,
                        format!("cannot open command log `{}`", path.display()),
                    )
                    .with_source(source)
                })?;
            (file, false)
        }
        Err(source) => {
            return Err(FirestoneError::new(
                kind,
                format!("cannot create command log `{}`", path.display()),
            )
            .with_source(source));
        }
    };
    let (file, was_created) = created;
    if was_created {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| {
                FirestoneError::new(
                    kind,
                    format!("cannot protect command log `{}`", path.display()),
                )
                .with_source(source)
            })?;
    }
    let metadata = file.metadata().map_err(|source| {
        FirestoneError::new(
            kind,
            format!("cannot inspect command log `{}`", path.display()),
        )
        .with_source(source)
    })?;
    if !metadata.is_file() {
        return Err(FirestoneError::new(
            kind,
            format!("command log `{}` is not a regular file", path.display()),
        ));
    }
    let mode = metadata.mode() & 0o7777;
    let uid = getuid().as_raw();
    if metadata.uid() != uid || mode != 0o600 {
        return Err(FirestoneError::new(
            kind,
            format!(
                "command log `{}` is insecure: expected uid {uid} and mode 0600, found uid {} and mode {mode:04o}",
                path.display(),
                metadata.uid()
            ),
        )
        .with_hint("replace the log with a mode-0600 regular file owned by the Firestone user"));
    }
    let current = fcntl(&file, FcntlArg::F_GETFL).map_err(|source| {
        FirestoneError::new(
            kind,
            format!("cannot inspect log flags for `{}`", path.display()),
        )
        .with_source(std::io::Error::from_raw_os_error(source as i32))
    })?;
    let mut open_flags = OFlag::from_bits_truncate(current);
    open_flags.remove(OFlag::O_NONBLOCK);
    fcntl(&file, FcntlArg::F_SETFL(open_flags)).map_err(|source| {
        FirestoneError::new(
            kind,
            format!("cannot clear nonblocking log flag for `{}`", path.display()),
        )
        .with_source(std::io::Error::from_raw_os_error(source as i32))
    })?;
    Ok(file)
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
    reader: JoinHandle<Result<Captured, std::io::Error>>,
    program: &std::ffi::OsStr,
    stream: &str,
    kind: ErrorKind,
    deadline: Option<Instant>,
    process_group: u32,
    timed_out: &mut bool,
) -> Result<Captured, FirestoneError> {
    match join_before_deadline(reader, deadline, process_group, timed_out) {
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

fn join_before_deadline<T>(
    worker: JoinHandle<T>,
    deadline: Option<Instant>,
    process_group: u32,
    timed_out: &mut bool,
) -> std::thread::Result<T> {
    if let Some(deadline) = deadline {
        while !worker.is_finished() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                *timed_out = true;
                let _ = kill_process_group(process_group);
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    }
    worker.join()
}

fn kill_process_group(process_group: u32) -> Result<(), std::io::Error> {
    let process_group = i32::try_from(process_group)
        .map(Pid::from_raw)
        .map_err(|_| std::io::Error::other("process id does not fit pid_t"))?;
    match killpg(process_group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(std::io::Error::from_raw_os_error(error as i32)),
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
            let mut bounded = characters
                .by_ref()
                .take(4096)
                .map(|character| {
                    if character.is_control() {
                        '\u{fffd}'
                    } else {
                        character
                    }
                })
                .collect::<String>();
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
    match (status.code(), status.signal()) {
        (Some(code), _) => code.to_string(),
        (None, Some(signal)) => format!("terminated by signal {signal}"),
        (None, None) => "terminated without an exit code or signal".to_owned(),
    }
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
    fn run_nonzero_after_stdin_closes_reports_status_and_tail()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(
            &dir,
            "closed-stdin",
            "i=1; while [ \"$i\" -le 12 ]; do printf 'line-%s\\n' \"$i\" >&2; i=$((i + 1)); done; exit 23",
        )?;

        let error = Cmd::new(&script)
            .stdin_bytes(vec![b'x'; 1024 * 1024])
            .run()
            .err()
            .ok_or("command should fail")?;

        assert!(error.message().contains("status 23"));
        assert!(error.message().contains("line-3"));
        assert!(error.message().contains("line-12"));
        assert!(error.hint().is_some());
        Ok(())
    }

    #[test]
    fn run_signaled_process_reports_signal_number() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "signaled", "kill -TERM $$")?;

        let error = Cmd::new(&script)
            .run()
            .err()
            .ok_or("signaled command should fail")?;

        assert!(error.message().contains("terminated by signal 15"));
        Ok(())
    }

    #[test]
    fn managed_process_exit_reports_program_status_and_safe_log_tail()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(
            &dir,
            "managed-failure",
            "printf 'line-1\\nline-2\\n\\033[31mline-3\\033[0m\\n' >&2; i=4; while [ \"$i\" -le 12 ]; do printf 'line-%s\\n' \"$i\" >&2; i=$((i + 1)); done; exit 23",
        )?;
        let stdout_log = dir.path().join("managed.stdout.log");
        let stderr_log = dir.path().join("managed.stderr.log");
        let mut process = Cmd::new(&script)
            .stdout_append(&stdout_log)
            .stderr_append(&stderr_log)
            .error_kind(crate::ErrorKind::Dependency)
            .spawn_process_group()?;
        let status = process.wait()?;

        let error = process.exit_error(
            status,
            "sidecar failed before readiness",
            "inspect the sidecar log and retry",
        );
        let message = error.message();
        assert_eq!(error.kind(), crate::ErrorKind::Dependency);
        assert!(message.contains(&script.display().to_string()));
        assert!(message.contains("status 23"));
        assert!(!message.contains("line-2\n"));
        assert!(message.contains("line-3"));
        assert!(message.contains("line-12"));
        assert!(!message.contains('\u{1b}'));
        assert!(message.contains('\u{fffd}'));
        assert_eq!(error.hint(), Some("inspect the sidecar log and retry"));
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
    fn log_targets_reject_symlinks_fifos_and_wrong_modes_without_blocking()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new()?;
        let real = dir.path().join("real.log");
        fs::write(&real, b"")?;
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600))?;
        let link = dir.path().join("link.log");
        symlink(&real, &link)?;
        let link_error = Cmd::new("/bin/true")
            .stderr_log(&link)
            .run()
            .err()
            .ok_or("symlink log target was accepted")?;
        assert_eq!(link_error.kind(), crate::ErrorKind::Generic);

        let fifo = dir.path().join("fifo.log");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )?;
        let started = Instant::now();
        let fifo_error = Cmd::new("/bin/true")
            .stderr_log(&fifo)
            .run()
            .err()
            .ok_or("FIFO log target was accepted")?;
        assert_eq!(fifo_error.kind(), crate::ErrorKind::Generic);
        assert!(started.elapsed() < Duration::from_secs(1));

        let permissive = dir.path().join("permissive.log");
        fs::write(&permissive, b"")?;
        fs::set_permissions(&permissive, fs::Permissions::from_mode(0o644))?;
        let mode_error = Cmd::new("/bin/true")
            .stderr_log(&permissive)
            .run()
            .err()
            .ok_or("wrong-mode log target was accepted")?;
        assert_eq!(mode_error.kind(), crate::ErrorKind::Generic);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn output_retries_executable_busy_until_writer_closes() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new()?;
        let script = executable(&dir, "busy-then-ready", "printf ready")?;
        let writable = std::fs::OpenOptions::new().write(true).open(&script)?;
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            drop(writable);
        });

        let output = Cmd::new(&script)
            .timeout(Duration::from_millis(500))
            .run()?;
        closer
            .join()
            .map_err(|_| std::io::Error::other("writer closer panicked"))?;
        assert_eq!(output.stdout(), b"ready");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writer_closing_at_deadline_boundary_cannot_enable_late_spawn()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "deadline-busy", "printf ran > \"$1\"")?;
        let marker = dir.path().join("late-marker");
        let writable = std::fs::OpenOptions::new().write(true).open(&script)?;
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            drop(writable);
        });
        let started = Instant::now();

        let error = Cmd::new(&script)
            .arg(marker.as_os_str())
            .timeout(Duration::from_millis(20))
            .run()
            .err()
            .ok_or("deadline-bound busy command unexpectedly started")?;
        closer
            .join()
            .map_err(|_| std::io::Error::other("writer closer panicked"))?;
        assert_eq!(error.kind(), crate::ErrorKind::Generic);
        assert!(error.message().contains("cannot start command"));
        assert!(!marker.exists());
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn persistent_executable_busy_is_contextual_and_deadline_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "always-busy", "exit 0")?;
        let _writable = std::fs::OpenOptions::new().write(true).open(&script)?;
        let started = Instant::now();

        let error = Cmd::new(&script)
            .timeout(Duration::from_millis(30))
            .run()
            .err()
            .ok_or("expected persistent executable-busy error")?;
        assert_eq!(error.kind(), crate::ErrorKind::Generic);
        assert!(error.message().contains("cannot start command"));
        assert!(error.message().contains(&script.display().to_string()));
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn output_timeout_kills_and_reaps_process() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "hang", "exec sleep 10")?;
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

    #[test]
    fn output_timeout_kills_pipe_inheriting_descendants() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new()?;
        let script = executable(&dir, "fork", "(sleep 10) & exit 0")?;
        let started = Instant::now();

        let error = match Cmd::new(script)
            .timeout(Duration::from_millis(100))
            .output()
        {
            Err(error) => error,
            Ok(_) => return Err(std::io::Error::other("capture should time out").into()),
        };

        assert_eq!(error.kind(), crate::ErrorKind::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn interactive_run_executes_with_arguments() -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let script = executable(&dir, "interactive", r#"printf done > "$1""#)?;
        let marker = dir.path().join("marker");

        Cmd::new(script)
            .arg(marker.as_os_str())
            .stdin_inherit()
            .run_interactive()?;

        assert_eq!(fs::read_to_string(marker)?, "done");
        Ok(())
    }

    #[test]
    fn interactive_run_nonzero_reports_program_and_status() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new()?;
        let script = executable(&dir, "interactive-failure", "exit 19")?;

        let error = match Cmd::new(&script).run_interactive() {
            Err(error) => error,
            Ok(()) => return Err(std::io::Error::other("command should fail").into()),
        };

        assert!(error.message().contains(&script.display().to_string()));
        assert!(error.message().contains("status 19"));
        Ok(())
    }

    #[test]
    fn interactive_run_rejects_buffered_stdin() {
        let error = Cmd::new("unused")
            .stdin_bytes(b"secret".to_vec())
            .run_interactive()
            .err();

        assert!(error.is_some());
        assert!(
            error
                .and_then(|error| error.hint().map(str::to_owned))
                .is_some()
        );
    }

    #[test]
    fn command_debug_redacts_secret_arguments_stdin_and_environment_values() {
        let command = Cmd::new("program")
            .arg("visible")
            .secret_arg("PRIVATE_KEY_MATERIAL")
            .env("TOKEN", "ENVIRONMENT_SECRET")
            .stdin_bytes(b"CLOUD_INIT_SECRET".to_vec());

        let debug = format!("{command:?}");

        assert!(debug.contains("visible"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("TOKEN"));
        assert!(!debug.contains("PRIVATE_KEY_MATERIAL"));
        assert!(!debug.contains("ENVIRONMENT_SECRET"));
        assert!(!debug.contains("CLOUD_INIT_SECRET"));
    }
}
