use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::{
    ErrorKind, Event, EventSink, FirestoneError, Paths, StepId, VsockPort, connect_vsock,
    readiness_ssh_plan,
};

const TRANSPORT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const SSH_PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SSH_CAPTURE_BYTES: usize = 64 * 1024;

/// Inputs for the M2 boot-heartbeat and SSH readiness contract.
pub struct ReadinessOptions<'a> {
    pub paths: &'a Paths,
    pub current_executable: &'a Path,
    pub name: &'a str,
    pub user: &'a str,
    pub first_boot: bool,
    pub started: Instant,
    pub deadline: Instant,
    pub cancelled: &'a AtomicBool,
}

/// Waits until bounded console growth is observed and BatchMode OpenSSH succeeds.
pub fn wait_for_ssh_ready(
    options: ReadinessOptions<'_>,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    let plan = readiness_ssh_plan(
        options.paths,
        options.current_executable,
        options.name,
        options.user,
    )?;
    let mut backend = SystemReadiness {
        paths: options.paths,
        name: options.name,
        plan,
    };
    wait_with_backend(&options, &mut backend, events)
}

trait ReadinessBackend {
    fn console_bytes(&mut self) -> Result<u64, FirestoneError>;
    fn transport_ready(&mut self, timeout: Duration) -> Result<bool, FirestoneError>;
    fn ssh_ready(&mut self, timeout: Duration) -> Result<SshProbe, FirestoneError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshProbe {
    Ready,
    Waiting,
    HostKeyChanged,
}

struct SystemReadiness<'a> {
    paths: &'a Paths,
    name: &'a str,
    plan: crate::SshCommandPlan,
}

impl ReadinessBackend for SystemReadiness<'_> {
    fn console_bytes(&mut self) -> Result<u64, FirestoneError> {
        let path = self.paths.machine_console_log(self.name)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                self.paths
                    .validate_owned_data_file(&path, "console log", 0o600, false)?;
                fs::metadata(&path)
                    .map(|metadata| metadata.len())
                    .map_err(|source| {
                        FirestoneError::new(
                            ErrorKind::Generic,
                            format!(
                                "cannot inspect boot heartbeat for machine `{}` at '{}'",
                                self.name,
                                path.display()
                            ),
                        )
                        .with_source(source)
                    })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(FirestoneError::new(
                ErrorKind::Generic,
                format!(
                    "cannot inspect boot heartbeat for machine `{}` at '{}'",
                    self.name,
                    path.display()
                ),
            )
            .with_source(source)),
        }
    }

    fn transport_ready(&mut self, timeout: Duration) -> Result<bool, FirestoneError> {
        match connect_vsock(self.paths, self.name, VsockPort::SSH, timeout) {
            Ok(connection) => {
                drop(connection);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotRunning | ErrorKind::Timeout | ErrorKind::Generic
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn ssh_ready(&mut self, timeout: Duration) -> Result<SshProbe, FirestoneError> {
        let output = match self
            .plan
            .command()
            .stdin_null()
            .timeout(timeout)
            .capture_limit(SSH_CAPTURE_BYTES)
            .error_kind(ErrorKind::Dependency)
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == ErrorKind::Timeout => return Ok(SshProbe::Waiting),
            Err(error) => return Err(error),
        };
        if output.success() {
            return Ok(SshProbe::Ready);
        }
        let stderr = output.stderr_lossy();
        if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
            || stderr.contains("Host key verification failed")
        {
            return Ok(SshProbe::HostKeyChanged);
        }
        Ok(SshProbe::Waiting)
    }
}

fn wait_with_backend(
    options: &ReadinessOptions<'_>,
    backend: &mut dyn ReadinessBackend,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    events.emit(Event::StepStart {
        id: StepId::from("boot"),
        label: "wait for guest boot".to_owned(),
    })?;

    let wait_reason = if options.first_boot {
        "waiting for cloud-init (first boot)"
    } else {
        "waiting for sshd on vsock"
    };
    let mut boot_done = false;
    let mut ssh_started = false;

    loop {
        if options.cancelled.load(Ordering::Relaxed) {
            return fail_wait(
                options,
                events,
                &mut boot_done,
                &mut ssh_started,
                wait_reason,
                interrupted_error(options.name),
            );
        }
        let now = Instant::now();
        if now >= options.deadline {
            return fail_wait(
                options,
                events,
                &mut boot_done,
                &mut ssh_started,
                wait_reason,
                readiness_timeout(options),
            );
        }

        let console_bytes = backend.console_bytes()?;
        let remaining = options.deadline.saturating_duration_since(now);
        let transport_timeout = TRANSPORT_PROBE_TIMEOUT.min(remaining);
        let transport_ready = backend.transport_ready(transport_timeout)?;

        if !boot_done && (console_bytes > 0 || transport_ready) {
            boot_done = true;
            events.emit(Event::StepDone {
                id: StepId::from("boot"),
                detail: Some("firmware+kernel".to_owned()),
                elapsed_ms: elapsed_millis(options.started.elapsed()),
            })?;
            start_ssh(events, &mut ssh_started, wait_reason)?;
        }

        if transport_ready {
            if !boot_done {
                boot_done = true;
                events.emit(Event::StepDone {
                    id: StepId::from("boot"),
                    detail: Some("firmware+kernel".to_owned()),
                    elapsed_ms: elapsed_millis(options.started.elapsed()),
                })?;
            }
            start_ssh(events, &mut ssh_started, wait_reason)?;
            let remaining = options.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                continue;
            }
            match backend.ssh_ready(SSH_PROBE_TIMEOUT.min(remaining))? {
                SshProbe::Ready => {
                    events.emit(Event::StepDone {
                        id: StepId::from("ssh"),
                        detail: Some("ready".to_owned()),
                        elapsed_ms: elapsed_millis(options.started.elapsed()),
                    })?;
                    return Ok(());
                }
                SshProbe::Waiting => {}
                SshProbe::HostKeyChanged => {
                    let error = FirestoneError::new(
                        ErrorKind::Conflict,
                        format!(
                            "SSH host key for machine `{}` changed without a cloud-init seed change",
                            options.name
                        ),
                    )
                    .with_hint(format!(
                        "inspect `firestone console {}` before changing its known_hosts trust",
                        options.name
                    ));
                    events.emit(Event::StepFail {
                        id: StepId::from("ssh"),
                        error: error.info(),
                    })?;
                    return Err(error);
                }
            }
        }

        let remaining = options.deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::sleep(READINESS_POLL_INTERVAL.min(remaining));
        }
    }
}

fn start_ssh(
    events: &mut dyn EventSink,
    started: &mut bool,
    wait_reason: &str,
) -> Result<(), FirestoneError> {
    if *started {
        return Ok(());
    }
    *started = true;
    events.emit(Event::StepStart {
        id: StepId::from("ssh"),
        label: "wait for SSH".to_owned(),
    })?;
    events.emit(Event::StepUpdate {
        id: StepId::from("ssh"),
        detail: wait_reason.to_owned(),
    })
}

fn fail_wait(
    options: &ReadinessOptions<'_>,
    events: &mut dyn EventSink,
    boot_done: &mut bool,
    ssh_started: &mut bool,
    wait_reason: &str,
    error: FirestoneError,
) -> Result<(), FirestoneError> {
    if !*boot_done {
        *boot_done = true;
        events.emit(Event::StepDone {
            id: StepId::from("boot"),
            detail: Some("no console heartbeat".to_owned()),
            elapsed_ms: elapsed_millis(options.started.elapsed()),
        })?;
    }
    start_ssh(events, ssh_started, wait_reason)?;
    events.emit(Event::StepFail {
        id: StepId::from("ssh"),
        error: error.info(),
    })?;
    Err(error)
}

fn readiness_timeout(options: &ReadinessOptions<'_>) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Timeout,
        format!(
            "machine `{}` did not become SSH-ready within the start timeout",
            options.name
        ),
    )
    .with_hint(format!(
        "inspect `firestone logs {0}` or attach with `firestone console {0}`; the VM is still running",
        options.name
    ))
}

fn interrupted_error(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Interrupted,
        format!("SSH readiness wait for machine `{name}` was interrupted"),
    )
    .with_hint(format!(
        "still booting in the background; `firestone stop {name}` to stop it"
    ))
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use crate::{ErrorKind, Event, PathInputs, Paths};

    use super::{ReadinessBackend, ReadinessOptions, SshProbe, wait_with_backend};

    struct FakeBackend {
        console: VecDeque<u64>,
        transport: VecDeque<bool>,
        ssh: VecDeque<SshProbe>,
        transport_calls: usize,
    }

    impl FakeBackend {
        fn new(console: &[u64], transport: &[bool], ssh: &[SshProbe]) -> Self {
            Self {
                console: console.iter().copied().collect(),
                transport: transport.iter().copied().collect(),
                ssh: ssh.iter().copied().collect(),
                transport_calls: 0,
            }
        }
    }

    impl ReadinessBackend for FakeBackend {
        fn console_bytes(&mut self) -> Result<u64, crate::FirestoneError> {
            Ok(self.console.pop_front().unwrap_or(0))
        }

        fn transport_ready(&mut self, _timeout: Duration) -> Result<bool, crate::FirestoneError> {
            self.transport_calls += 1;
            Ok(self.transport.pop_front().unwrap_or(false))
        }

        fn ssh_ready(&mut self, _timeout: Duration) -> Result<SshProbe, crate::FirestoneError> {
            Ok(self.ssh.pop_front().unwrap_or(SshProbe::Waiting))
        }
    }

    struct Fixture {
        _temp: TempDir,
        paths: Paths,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let temp = TempDir::new()?;
            let root = fs::canonicalize(temp.path())?;
            let paths = Paths::from_inputs(&PathInputs {
                current_dir: root.clone(),
                home_dir: None,
                firestone_home: Some(root.join("home")),
                firestone_config_dir: None,
                firestone_data_dir: None,
                firestone_runtime_dir: None,
                xdg_config_home: None,
                xdg_data_home: None,
                xdg_runtime_dir: None,
                uid: nix::unistd::getuid().as_raw(),
            })?;
            Ok(Self { _temp: temp, paths })
        }
    }

    #[test]
    fn readiness_console_then_transport_emits_ordered_first_boot_transition()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let options = ReadinessOptions {
            paths: &fixture.paths,
            current_executable: std::path::Path::new("firestone"),
            name: "demo",
            user: "root",
            first_boot: true,
            started,
            deadline: started + Duration::from_secs(2),
            cancelled: &cancelled,
        };
        let mut backend = FakeBackend::new(&[0, 16], &[false, true], &[SshProbe::Ready]);
        let mut events = Vec::new();
        wait_with_backend(&options, &mut backend, &mut events)?;

        assert!(started.elapsed() >= Duration::from_millis(90));
        assert_eq!(backend.transport_calls, 2);
        let starts = events
            .iter()
            .filter_map(|event| match event {
                Event::StepStart { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(starts, vec!["boot", "ssh"]);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::StepUpdate { id, detail }
                if id.as_str() == "ssh" && detail == "waiting for cloud-init (first boot)"
        )));
        assert!(matches!(
            events.last(),
            Some(Event::StepDone { id, detail: Some(detail), .. })
                if id.as_str() == "ssh" && detail == "ready"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
    }

    #[test]
    fn readiness_timeout_is_bounded_and_has_stable_recovery_hints()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();
        let options = ReadinessOptions {
            paths: &fixture.paths,
            current_executable: std::path::Path::new("firestone"),
            name: "demo",
            user: "root",
            first_boot: false,
            started,
            deadline: started + Duration::from_millis(180),
            cancelled: &cancelled,
        };
        let mut backend = FakeBackend::new(&[], &[], &[]);
        let mut events = Vec::new();
        let error = wait_with_backend(&options, &mut backend, &mut events)
            .err()
            .ok_or("readiness timeout unexpectedly succeeded")?;
        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert!(error.hint().is_some_and(|hint| {
            hint.contains("firestone logs demo") && hint.contains("firestone console demo")
        }));
        assert!(started.elapsed() >= Duration::from_millis(170));
        assert!(started.elapsed() < Duration::from_millis(600));
        assert!(matches!(
            events.last(),
            Some(Event::StepFail { id, error })
                if id.as_str() == "ssh" && error.kind == ErrorKind::Timeout
        ));
        Ok(())
    }

    #[test]
    fn readiness_cancellation_stops_polling_without_terminal_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            setter.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let options = ReadinessOptions {
            paths: &fixture.paths,
            current_executable: std::path::Path::new("firestone"),
            name: "demo",
            user: "root",
            first_boot: false,
            started,
            deadline: started + Duration::from_secs(2),
            cancelled: &cancelled,
        };
        let mut backend = FakeBackend::new(&[], &[], &[]);
        let mut events = Vec::new();
        let error = wait_with_backend(&options, &mut backend, &mut events)
            .err()
            .ok_or("cancelled readiness unexpectedly succeeded")?;
        worker.join().map_err(|_| "cancellation worker panicked")?;
        assert_eq!(error.kind(), ErrorKind::Interrupted);
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("still booting in the background"))
        );
        assert!(started.elapsed() < Duration::from_millis(600));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Result { .. }))
        );
        Ok(())
    }
}
