use std::{
    collections::BTreeMap,
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use firestone_core::{
    Arch, Catalog, CloudInitSpec, Cmd, DependencyManifest, ErrorKind, Event, ExitReason,
    FirestoneError, Firmware, ImageRef, ImageStore, MachineLock, MachineSpec, MachineState,
    MachineStatus, ManagedProcess, NetMode, NetworkSpec, PathInputs, Paths, ProcessSignal,
    ShimClient, ShimTimeouts, StateImage, StateStore, StateVersion, VmmSpec, atomic,
    launch_prepared, prepare_start,
};
use fs2::FileExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const NAME: &str = "shim-test";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const VMCONFIG: &[u8] = br#"{"console":"exact-persisted-bytes","z":1}"#;

struct BuiltFakeVmm {
    _root: TempDir,
    path: PathBuf,
}

static BUILT_FAKE_VMM: OnceLock<Result<BuiltFakeVmm, String>> = OnceLock::new();

struct Fixture {
    _root: TempDir,
    paths: Paths,
    shim: Option<ManagedProcess>,
    record: PathBuf,
    body: PathBuf,
    descendant_pid: PathBuf,
}

impl Fixture {
    fn spawn(behavior: &str, initial_console: Option<&[u8]>, descendant: bool) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        let fake_vmm = shared_fake_vmm()?;
        let mut inputs = PathInputs::capture()?;
        inputs.firestone_home = Some(fs::canonicalize(root.path())?.join("home"));
        let paths = Paths::from_inputs(&inputs)?;
        paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
        paths.ensure_owned_data_directory(&paths.machines_dir(), "machines directory", false)?;
        paths.ensure_owned_data_directory(&paths.machine_dir(NAME)?, "machine directory", false)?;
        paths.ensure_runtime_dir()?;
        paths.ensure_machine_runtime_dir(NAME)?;
        paths.clear_machine_runtime_dir(NAME, false)?;

        let record = root.path().join("requests.log");
        let body = root.path().join("create-body.json");
        let descendant_pid = root.path().join("descendant.pid");
        let console = paths.machine_console_log(NAME)?;
        if let Some(contents) = initial_console {
            fs::write(&console, contents)?;
            fs::set_permissions(&console, fs::Permissions::from_mode(0o600))?;
        }
        atomic::write(&paths.machine_vmconfig(NAME)?, VMCONFIG)?;

        let mut extra_args = vec![
            "--record".to_owned(),
            record.to_string_lossy().into_owned(),
            "--body".to_owned(),
            body.to_string_lossy().into_owned(),
            "--behavior".to_owned(),
            behavior.to_owned(),
            "--console-log".to_owned(),
            console.to_string_lossy().into_owned(),
        ];
        if descendant {
            extra_args.push("--descendant-pid".to_owned());
            extra_args.push(descendant_pid.to_string_lossy().into_owned());
        }
        let plan = json!({
            "version": 1,
            "name": NAME,
            "vmm_binary": fake_vmm,
            "vmm_binary_sha256": sha256_file(&fake_vmm)?,
            "vmm_extra_args": extra_args,
            "vmconfig_sha256": sha256_bytes(VMCONFIG),
            "vmconfig_len": VMCONFIG.len(),
            "api_timeout_ms": 250,
            "readiness_timeout_ms": 3000,
            "control_io_timeout_ms": 150,
            "launch_request_timeout_ms": 5000,
        });
        atomic::write_json_with_mode(&paths.machine_shim_plan(NAME)?, &plan, 0o600)?;

        let mut events = Vec::new();
        let lock = MachineLock::acquire(NAME, &paths.machine_lock(NAME)?, &mut events)?;
        let mut state = base_state(&paths)?;
        StateStore::new(paths.machine_state(NAME)?).write_from_locked_action(&state, &lock)?;
        atomic::write(&paths.machine_spec(NAME)?, b"image = \"test\"\n")?;

        let shim_log = paths.machine_shim_log(NAME)?;
        let mut shim = Cmd::new(env!("CARGO_BIN_EXE_firestone"))
            .arg("_shim")
            .arg(NAME)
            .cwd("/")
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env("FIRESTONE_CONFIG_DIR", paths.config_dir().as_os_str())
            .env("FIRESTONE_DATA_DIR", paths.data_dir().as_os_str())
            .env("FIRESTONE_RUNTIME_DIR", paths.runtime_dir().as_os_str())
            .stdin_null()
            .stdout_append(&shim_log)
            .stderr_append(&shim_log)
            .spawn_session_candidate()?;
        state.shim_pid = Some(shim.id());
        StateStore::new(paths.machine_state(NAME)?).write_from_locked_action(&state, &lock)?;
        drop(lock);
        wait_for_path(&paths.machine_shim_socket(NAME)?, Duration::from_secs(3))?;
        shim.confirm_session()?;

        Ok(Self {
            _root: root,
            paths,
            shim: Some(shim),
            record,
            body,
            descendant_pid,
        })
    }

    fn client(&self) -> ShimClient {
        ShimClient::new(
            self.paths.machine_shim_socket(NAME).unwrap_or_default(),
            CONTROL_TIMEOUT,
        )
    }

    fn launch(&self) -> Result<Vec<Event>, firestone_core::FirestoneError> {
        let mut events = Vec::new();
        if let Err(error) = self.client().launch(&mut events) {
            let log = fs::read_to_string(self.paths.machine_shim_log(NAME).unwrap_or_default())
                .unwrap_or_default();
            eprintln!("launch error: {error:?}\nshim log:\n{log}");
            return Err(error);
        }
        Ok(events)
    }
    fn wait_shim(&mut self) -> TestResult<()> {
        let Some(process) = self.shim.as_mut() else {
            return Ok(());
        };
        let status = process
            .wait_timeout(Duration::from_secs(5))?
            .ok_or("shim did not exit")?;
        self.shim = None;
        if status.success() {
            Ok(())
        } else {
            let log = fs::read_to_string(self.paths.machine_shim_log(NAME)?)?;
            Err(format!("shim exited {status:?}: {log}").into())
        }
    }

    fn stop(&mut self, timeout: Duration, force: bool) -> TestResult<MachineState> {
        let mut events = Vec::new();
        self.client().stop(timeout, force, &mut events)?;
        self.wait_shim()?;
        Ok(StateStore::new(self.paths.machine_state(NAME)?).read()?)
    }

    fn wait_for_status(
        &self,
        status: MachineStatus,
        timeout: Duration,
    ) -> TestResult<MachineState> {
        let deadline = Instant::now() + timeout;
        loop {
            let state = StateStore::new(self.paths.machine_state(NAME)?).read()?;
            if state.status == status {
                return Ok(state);
            }
            if Instant::now() >= deadline {
                return Err(
                    format!("state did not become {status:?}; found {:?}", state.status).into(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn start_preparation_orders_image_overlay_seed_vmconfig_and_plan() -> TestResult<()> {
    let root = tempfile::tempdir()?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
    let fake = shared_fake_vmm()?;
    let mut inputs = PathInputs::capture()?;
    inputs.firestone_home = Some(fs::canonicalize(root.path())?.join("h"));
    let paths = Paths::from_inputs(&inputs)?;
    paths.ensure_owned_data_directory(paths.data_dir(), "data directory", true)?;
    paths.ensure_owned_data_directory(&paths.machines_dir(), "machines directory", false)?;
    paths.ensure_owned_data_directory(&paths.machine_dir(NAME)?, "machine directory", false)?;

    let source = root.path().join("source.raw");
    fs::write(&source, b"raw-machine-image")?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
    let firmware = root.path().join("firmware.fd");
    fs::write(&firmware, b"firmware")?;
    fs::set_permissions(&firmware, fs::Permissions::from_mode(0o600))?;

    let mut spec = MachineSpec {
        image: ImageRef::new(source.to_string_lossy().into_owned()),
        arch: Some(Arch::X86_64),
        network: NetworkSpec {
            mode: NetMode::None,
            ..NetworkSpec::default()
        },
        cloud_init: CloudInitSpec {
            provisioning: false,
            ..CloudInitSpec::default()
        },
        vmm: VmmSpec {
            binary: Some(fake.clone()),
            firmware: Firmware::path(&firmware)?,
            ..VmmSpec::default()
        },
        ..MachineSpec::default()
    };
    let state = MachineState {
        version: StateVersion,
        status: MachineStatus::Created,
        image: StateImage {
            r#ref: source.to_string_lossy().into_owned(),
            id: None,
            sha256: None,
        },
        mac: None,
        cid: 3,
        instance_id: None,
        shim_pid: None,
        vmm_pid: None,
        sidecar_pids: BTreeMap::new(),
        runtime_dir: paths.machine_runtime_dir(NAME)?,
        started_at: None,
        forwards: Vec::new(),
        degraded: Vec::new(),
        last_exit: None,
    };
    let mut events = Vec::new();
    let lock = MachineLock::acquire(NAME, &paths.machine_lock(NAME)?, &mut events)?;
    StateStore::new(paths.machine_state(NAME)?).write_from_locked_action(&state, &lock)?;
    let store = ImageStore::new(paths.clone(), Catalog::built_in()?, Arch::X86_64, &fake)?;
    let prepared = prepare_start(
        &paths,
        &store,
        &DependencyManifest::bundled()?,
        NAME,
        &spec,
        state,
        root.path(),
        &lock,
        &mut events,
        ShimTimeouts::default(),
    )?;

    assert_eq!(prepared.state().status, MachineStatus::Created);
    assert!(prepared.state().image.id.is_some());
    assert!(prepared.state().image.sha256.is_some());
    assert!(prepared.state().instance_id.is_some());
    assert!(prepared.state().mac.is_some());
    assert!(paths.machine_disk(NAME)?.exists());
    assert!(paths.machine_seed_image(NAME)?.exists());
    assert!(paths.machine_vmconfig(NAME)?.exists());
    assert!(paths.machine_shim_plan(NAME)?.exists());
    let vmconfig: serde_json::Value =
        serde_json::from_slice(&fs::read(paths.machine_vmconfig(NAME)?)?)?;
    assert_eq!(
        vmconfig["payload"]["firmware"],
        firmware.to_string_lossy().as_ref()
    );
    assert!(vmconfig.get("net").is_none());
    assert!(vmconfig.get("fs").is_none());

    let qemu_log = fs::read_to_string(fake.with_extension("qemu.log"))?;
    assert_order(
        &qemu_log,
        &["convert -f raw -O qcow2", "create -f qcow2 -F qcow2 -b"],
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::StepDone { id, .. } if id.as_str() == "seed"))
    );

    paths.clear_machine_runtime_dir(NAME, true)?;
    let quick = ShimTimeouts {
        api: Duration::from_millis(50),
        readiness: Duration::from_millis(100),
        control_io: Duration::from_millis(50),
        launch_request: Duration::from_millis(100),
    };
    spec.cloud_init.user_data = Some(root.path().join("deferred-user-data"));
    let error = require_firestone_error(
        prepare_start(
            &paths,
            &store,
            &DependencyManifest::bundled()?,
            NAME,
            &spec,
            prepared.state().clone(),
            root.path(),
            &lock,
            &mut events,
            quick,
        ),
        "deferred user-data must fail after image preparation",
    )?;
    assert_eq!(error.kind(), ErrorKind::InvalidSpec);
    assert!(!paths.machine_runtime_dir(NAME)?.exists());
    assert_eq!(
        StateStore::new(paths.machine_state(NAME)?).read()?.status,
        MachineStatus::Created
    );

    spec.cloud_init.user_data = None;
    spec.vmm.firmware = Firmware::path(root.path().join("missing-firmware"))?;
    let error = require_firestone_error(
        prepare_start(
            &paths,
            &store,
            &DependencyManifest::bundled()?,
            NAME,
            &spec,
            prepared.state().clone(),
            root.path(),
            &lock,
            &mut events,
            quick,
        ),
        "missing firmware must fail before runtime publication",
    )?;
    assert!(matches!(
        error.kind(),
        ErrorKind::Dependency | ErrorKind::InvalidSpec
    ));
    assert!(!paths.machine_runtime_dir(NAME)?.exists());

    spec.vmm.firmware = Firmware::path(&firmware)?;
    let launch_record = root.path().join("prepared-launch.log");
    let launch_body = root.path().join("prepared-body.json");
    spec.vmm.extra_args = vec![
        "--record".to_owned(),
        launch_record.to_string_lossy().into_owned(),
        "--body".to_owned(),
        launch_body.to_string_lossy().into_owned(),
        "--behavior".to_owned(),
        "normal".to_owned(),
        "--console-log".to_owned(),
        paths
            .machine_console_log(NAME)?
            .to_string_lossy()
            .into_owned(),
    ];
    let ready = prepare_start(
        &paths,
        &store,
        &DependencyManifest::bundled()?,
        NAME,
        &spec,
        prepared.state().clone(),
        root.path(),
        &lock,
        &mut events,
        ShimTimeouts {
            api: Duration::from_millis(250),
            readiness: Duration::from_secs(2),
            control_io: Duration::from_millis(500),
            launch_request: Duration::from_secs(3),
        },
    )?;
    let running = match launch_prepared(
        &paths,
        Path::new(env!("CARGO_BIN_EXE_firestone")),
        ready,
        lock,
        &mut events,
    ) {
        Ok(running) => running,
        Err(error) => {
            let log = fs::read_to_string(paths.machine_shim_log(NAME)?).unwrap_or_default();
            eprintln!(
                "lifecycle launch error: {error:?}
shim log:
{log}"
            );
            return Err(error.into());
        }
    };
    assert_eq!(running.status, MachineStatus::Running);
    assert_eq!(
        fs::read(launch_body)?,
        fs::read(paths.machine_vmconfig(NAME)?)?
    );
    let mut stop_events = Vec::new();
    ShimClient::new(paths.machine_shim_socket(NAME)?, Duration::from_secs(2)).stop(
        Duration::from_secs(2),
        false,
        &mut stop_events,
    )?;
    let stopped = StateStore::new(paths.machine_state(NAME)?).read()?;
    assert_eq!(stopped.status, MachineStatus::Stopped);
    assert!(!paths.machine_runtime_dir(NAME)?.exists());

    Ok(())
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(process) = self.shim.as_mut() {
            let _ = process.signal_group(ProcessSignal::Kill);
            let _ = process.signal_process(ProcessSignal::Kill);
            let _ = process.wait_timeout(Duration::from_secs(2));
        }
    }
}

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

fn require_firestone_error<T>(
    result: Result<T, FirestoneError>,
    label: &str,
) -> TestResult<FirestoneError> {
    match result {
        Err(error) => Ok(error),
        Ok(_) => Err(label.to_owned().into()),
    }
}

#[test]
fn shim_protocol_launch_status_stop_preserves_exact_bytes_and_logs() -> TestResult<()> {
    let mut fixture = Fixture::spawn("normal", Some(b"previous boot\n"), false)?;
    let starting = fixture.client().status()?;
    assert_eq!(starting.status, MachineStatus::Starting);
    assert_eq!(
        starting.pids.shim,
        fixture.shim.as_ref().ok_or("missing shim")?.id()
    );
    fixture.client().ping()?;

    let events = fixture.launch()?;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::StepDone { id, .. } if id.as_str() == "vmm"))
    );
    let running = fixture.client().status()?;
    assert_eq!(running.status, MachineStatus::Running);
    assert!(running.pids.vmm.is_some());
    assert!(running.pids.sidecars.is_empty());
    assert_eq!(fs::read(&fixture.body)?, VMCONFIG);

    let record = fs::read_to_string(&fixture.record)?;
    assert_order(
        &record,
        &[
            "GET /api/v1/vmm.ping",
            "PUT /api/v1/vm.create",
            "PUT /api/v1/vm.boot",
        ],
    );
    assert!(record.contains("\"--api-socket\""));
    assert!(record.contains("\"--log-file\""));
    assert!(!record.contains("FIRESTONE_"));

    let state = fixture.stop(Duration::from_secs(2), false)?;
    assert_eq!(state.status, MachineStatus::Stopped);
    assert_eq!(
        state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(&ExitReason::GuestShutdown)
    );
    assert_eq!(
        fs::read(fixture.paths.machine_console_log(NAME)?)?,
        b"previous boot\ncurrent boot\n"
    );
    assert!(!fixture.paths.machine_runtime_dir(NAME)?.exists());
    Ok(())
}

#[test]
fn shim_control_malformed_oversized_slow_and_duplicate_frames_leave_server_live() -> TestResult<()>
{
    let mut fixture = Fixture::spawn("normal", None, false)?;
    for request in [b"{}\n".as_slice(), &[b'x'; 4097][..]] {
        let response = raw_request(&fixture.paths.machine_shim_socket(NAME)?, request)?;
        assert!(response.contains("\"ok\":false"));
    }

    let socket = fixture.paths.machine_shim_socket(NAME)?;
    let mut slow = UnixStream::connect(&socket)?;
    slow.write_all(br#"{"op":"sta"#)?;
    thread::sleep(Duration::from_millis(300));
    let mut response = String::new();
    BufReader::new(slow).read_line(&mut response)?;
    assert!(response.contains("\"ok\":false"));
    fixture.client().ping()?;

    fixture.launch()?;
    let mut duplicate_events = Vec::new();
    let duplicate = require_firestone_error(
        fixture.client().launch(&mut duplicate_events),
        "duplicate launch unexpectedly succeeded",
    )?;
    assert_eq!(duplicate.kind(), ErrorKind::AlreadyRunning);
    let _ = fixture.stop(Duration::from_secs(2), false)?;
    Ok(())
}

#[test]
fn shim_launch_client_disconnect_continues_and_lifetime_lock_contends() -> TestResult<()> {
    let mut fixture = Fixture::spawn("normal", None, false)?;
    let socket = fixture.paths.machine_shim_socket(NAME)?;
    let mut launch = UnixStream::connect(&socket)?;
    launch.write_all(b"{\"op\":\"launch\"}\n")?;
    drop(launch);

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match fixture.client().status() {
            Ok(status) if status.status == MachineStatus::Running => break,
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                return Err(format!("shim did not continue after disconnect: {result:?}").into());
            }
        }
    }

    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.paths.machine_lock(NAME)?)?;
    assert!(lock_file.try_lock_exclusive().is_err());
    let _ = fixture.stop(Duration::from_secs(2), false)?;
    lock_file.try_lock_exclusive()?;
    lock_file.unlock()?;
    Ok(())
}

#[test]
fn shim_launch_failure_boundaries_roll_back_to_failed_and_clean_runtime() -> TestResult<()> {
    for behavior in ["exit-before-api", "never-ready", "create-fail", "boot-fail"] {
        let mut fixture = Fixture::spawn(behavior, Some(b"old\n"), false)?;
        let error = require_firestone_error(fixture.launch(), "injected launch succeeded")?;
        assert!(matches!(
            error.kind(),
            ErrorKind::Generic | ErrorKind::Timeout
        ));
        fixture.wait_shim().or_else(|error| {
            let state = StateStore::new(fixture.paths.machine_state(NAME)?).read()?;
            if state.status == MachineStatus::Failed {
                Ok(())
            } else {
                Err(error)
            }
        })?;
        let state = StateStore::new(fixture.paths.machine_state(NAME)?).read()?;
        assert_eq!(state.status, MachineStatus::Failed, "behavior {behavior}");
        assert!(state.last_exit.is_some());
        assert!(!fixture.paths.machine_runtime_dir(NAME)?.exists());
        assert!(fs::read(fixture.paths.machine_console_log(NAME)?)?.starts_with(b"old\n"));
    }
    Ok(())
}

#[test]
fn shim_graceful_escalated_force_and_descendant_cleanup_are_deterministic() -> TestResult<()> {
    let mut graceful = Fixture::spawn("normal", None, false)?;
    graceful.launch()?;
    let graceful_state = graceful.stop(Duration::from_secs(2), false)?;
    assert_eq!(
        graceful_state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(&ExitReason::GuestShutdown)
    );

    let mut info_shutdown = Fixture::spawn("info-shutdown", None, false)?;
    info_shutdown.launch()?;
    let info_state = info_shutdown.stop(Duration::from_secs(2), false)?;
    assert_eq!(
        info_state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(&ExitReason::GuestShutdown)
    );
    let info_record = fs::read_to_string(&info_shutdown.record)?;
    assert_order(
        &info_record,
        &[
            "PUT /api/v1/vm.power-button",
            "GET /api/v1/vm.info",
            "PUT /api/v1/vmm.shutdown",
        ],
    );

    let mut api_failure = Fixture::spawn("power-fail", None, false)?;
    api_failure.launch()?;
    let api_failure_state = api_failure.stop(Duration::from_secs(2), false)?;
    assert!(matches!(
        api_failure_state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(ExitReason::Failure(reason)) if reason == "VMM API failed during graceful stop"
    ));

    let mut escalated = Fixture::spawn("ignore-power", None, false)?;
    escalated.launch()?;
    let escalated_state = escalated.stop(Duration::ZERO, false)?;
    assert!(matches!(
        escalated_state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(ExitReason::Failure(reason)) if reason == "graceful stop timed out"
    ));

    let mut forced = Fixture::spawn("ignore-power", None, true)?;
    forced.launch()?;
    wait_for_path(&forced.descendant_pid, Duration::from_secs(2))?;
    let _descendant = fs::read_to_string(&forced.descendant_pid)?
        .trim()
        .parse::<u32>()?;
    let forced_state = forced.stop(Duration::ZERO, true)?;
    assert!(matches!(
        forced_state.last_exit.as_ref().map(|exit| &exit.reason),
        Some(ExitReason::Failure(reason)) if reason == "forced stop"
    ));
    #[cfg(target_os = "linux")]
    assert!(
        !PathBuf::from("/proc")
            .join(_descendant.to_string())
            .exists()
    );
    Ok(())
}

#[test]
fn shim_spontaneous_exit_signal_and_atomic_state_paths_finish_once() -> TestResult<()> {
    let mut spontaneous = Fixture::spawn("spontaneous", None, false)?;
    let _ = spontaneous.launch();
    spontaneous
        .wait_shim()
        .or_else(|_| Ok::<(), Box<dyn std::error::Error>>(()))?;
    let failed = spontaneous.wait_for_status(MachineStatus::Failed, Duration::from_secs(2))?;
    assert!(matches!(
        failed.last_exit.as_ref().map(|exit| &exit.reason),
        Some(ExitReason::Failure(_))
    ));
    assert!(!spontaneous.paths.machine_runtime_dir(NAME)?.exists());

    let mut signalled = Fixture::spawn("normal", None, false)?;
    signalled.launch()?;
    let state_path = signalled.paths.machine_state(NAME)?;
    let reading = Arc::new(AtomicBool::new(true));
    let reader_flag = Arc::clone(&reading);
    let reader_path = state_path.clone();
    let reader = thread::spawn(move || {
        let store = StateStore::new(reader_path);
        let mut reads = 0_u64;
        while reader_flag.load(Ordering::Relaxed) {
            store.read()?;
            reads += 1;
        }
        Ok::<u64, firestone_core::FirestoneError>(reads)
    });
    signalled
        .shim
        .as_ref()
        .ok_or("missing shim")?
        .signal_process(ProcessSignal::Terminate)?;
    signalled.wait_shim()?;
    reading.store(false, Ordering::Relaxed);
    let reads = reader.join().map_err(|_| "state reader panicked")??;
    assert!(reads > 0);
    let stopped = StateStore::new(state_path).read()?;
    assert_eq!(stopped.status, MachineStatus::Stopped);
    assert!(stopped.last_exit.is_some());
    assert!(!signalled.paths.machine_runtime_dir(NAME)?.exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn shim_crash_api_stop_uses_persisted_identity_without_pid_reuse() -> TestResult<()> {
    let mut fixture = Fixture::spawn("normal", None, false)?;
    fixture.launch()?;
    let running = StateStore::new(fixture.paths.machine_state(NAME)?).read()?;
    let vmm_pid = running.vmm_pid.ok_or("missing VMM pid")?;
    let mut shim = fixture.shim.take().ok_or("missing shim process")?;
    shim.signal_process(ProcessSignal::Kill)?;
    let _ = shim
        .wait_timeout(Duration::from_secs(3))?
        .ok_or("killed shim was not reaped")?;

    let mut events = Vec::new();
    let lock = MachineLock::acquire(NAME, &fixture.paths.machine_lock(NAME)?, &mut events)?;
    let stopped = firestone_core::stop_unsupervised(
        &fixture.paths,
        NAME,
        running,
        &lock,
        Duration::from_secs(2),
        false,
        &mut events,
    )?;
    assert_eq!(stopped.status, MachineStatus::Stopped);
    assert!(stopped.shim_pid.is_none());
    assert!(stopped.vmm_pid.is_none());
    assert!(!PathBuf::from("/proc").join(vmm_pid.to_string()).exists());
    assert!(!fixture.paths.machine_runtime_dir(NAME)?.exists());
    drop(lock);
    Ok(())
}

fn base_state(paths: &Paths) -> TestResult<MachineState> {
    Ok(MachineState {
        version: StateVersion,
        status: MachineStatus::Starting,
        image: StateImage {
            r#ref: "test:image".to_owned(),
            id: None,
            sha256: None,
        },
        mac: Some("52:54:00:12:34:56".to_owned()),
        cid: 3,
        instance_id: Some("iid-shim-test".to_owned()),
        shim_pid: None,
        vmm_pid: None,
        sidecar_pids: BTreeMap::new(),
        runtime_dir: paths.machine_runtime_dir(NAME)?,
        started_at: Some("2026-08-28T00:00:00Z".to_owned()),
        forwards: Vec::new(),
        degraded: Vec::new(),
        last_exit: None,
    })
}

fn shared_fake_vmm() -> TestResult<PathBuf> {
    match BUILT_FAKE_VMM.get_or_init(|| build_shared_fake_vmm().map_err(|error| error.to_string()))
    {
        Ok(built) => Ok(built.path.clone()),
        Err(error) => Err(error.clone().into()),
    }
}

fn build_shared_fake_vmm() -> TestResult<BuiltFakeVmm> {
    let root = tempfile::tempdir()?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
    let path = compile_fake_vmm(root.path())?;
    Ok(BuiltFakeVmm { _root: root, path })
}

fn compile_fake_vmm(directory: &Path) -> TestResult<PathBuf> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_vmm.rs");
    let binary = directory.join("fake-vmm");
    Cmd::new("rustc")
        .arg("--edition=2024")
        .arg(source.as_os_str())
        .arg("-o")
        .arg(binary.as_os_str())
        .timeout(Duration::from_secs(30))
        .run()?;
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
    Ok(binary)
}

fn sha256_file(path: &Path) -> TestResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn wait_for_path(path: &Path, timeout: Duration) -> TestResult<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!("{} did not appear", path.display()).into())
}

fn raw_request(socket: &Path, bytes: &[u8]) -> TestResult<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(bytes)?;
    if !bytes.ends_with(b"\n") {
        stream.write_all(b"\n")?;
    }
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    Ok(response)
}

fn assert_order(haystack: &str, needles: &[&str]) {
    let mut offset = 0;
    for needle in needles {
        let relative = haystack[offset..]
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {haystack:?}"));
        offset += relative + needle.len();
    }
}
