use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg},
};

use crate::{ErrorKind, Event, EventSink, FirestoneError, Level};

const WAIT_NOTICE_AFTER: Duration = Duration::from_secs(1);
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy)]
struct LockTiming {
    wait_notice_after: Duration,
    timeout: Duration,
    poll_interval: Duration,
}

impl LockTiming {
    const PRODUCTION: Self = Self {
        wait_notice_after: WAIT_NOTICE_AFTER,
        timeout: LOCK_TIMEOUT,
        poll_interval: POLL_INTERVAL,
    };
}

/// An exclusive advisory lock for one machine directory.
///
/// Mutating CLI and REST actions keep this guard alive for their full
/// duration. State writes performed outside a live shim require this guard.
#[derive(Debug)]
pub struct MachineLock {
    _file: Flock<File>,
    path: PathBuf,
}

impl MachineLock {
    /// Acquires the machine lock, waiting at most ten seconds.
    pub fn acquire(
        name: &str,
        path: &Path,
        events: &mut dyn EventSink,
    ) -> Result<Self, FirestoneError> {
        Self::acquire_with_timing(name, path, events, LockTiming::PRODUCTION)
    }

    fn acquire_with_timing(
        name: &str,
        path: &Path,
        events: &mut dyn EventSink,
        timing: LockTiming,
    ) -> Result<Self, FirestoneError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| lock_io_failure("open", path, error))?;
        let started = Instant::now();
        let mut notice_emitted = false;
        let mut contended = false;

        loop {
            let elapsed = started.elapsed();
            if contended && !notice_emitted && elapsed >= timing.wait_notice_after {
                events.emit(Event::Log {
                    level: Level::Info,
                    message: format!("waiting for another firestone operation on `{name}`"),
                })?;
                notice_emitted = true;
            }

            if contended && elapsed >= timing.timeout {
                return Err(FirestoneError::new(
                    ErrorKind::Busy,
                    format!("machine `{name}` is busy with another firestone operation"),
                )
                .with_hint("wait for the other operation to finish and retry"));
            }

            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(file) => {
                    return Ok(Self {
                        _file: file,
                        path: path.to_path_buf(),
                    });
                }
                Err((returned_file, error))
                    if error == Errno::EWOULDBLOCK || error == Errno::EAGAIN =>
                {
                    file = returned_file;
                    contended = true;
                }
                Err((_, error)) => {
                    return Err(lock_io_failure("acquire", path, io::Error::from(error)));
                }
            }

            let elapsed = started.elapsed();
            let until_timeout = timing.timeout.saturating_sub(elapsed);
            let until_notice = if notice_emitted {
                timing.poll_interval
            } else {
                timing.wait_notice_after.saturating_sub(elapsed)
            };
            let sleep_for = timing.poll_interval.min(until_timeout).min(until_notice);
            if !sleep_for.is_zero() {
                thread::sleep(sleep_for);
            }
        }
    }

    /// Returns the exact lock file held by this guard.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_io_failure(operation: &'static str, path: &Path, error: io::Error) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::Generic,
        format!("cannot {operation} machine lock {}", path.display()),
    )
    .with_hint("check that the machine directory is writable")
    .with_source(error)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::Path,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use crate::{ErrorKind, Event, Level};

    use super::{LockTiming, MachineLock};

    const HELPER_LOCK_PATH: &str = "FIRESTONE_TEST_HELPER_LOCK_PATH";
    const HELPER_READY_PATH: &str = "FIRESTONE_TEST_HELPER_READY_PATH";
    const HELPER_RELEASE_PATH: &str = "FIRESTONE_TEST_HELPER_RELEASE_PATH";

    #[test]
    fn lock_helper_process() -> Result<(), Box<dyn std::error::Error>> {
        let Ok(lock_path) = env::var(HELPER_LOCK_PATH) else {
            return Ok(());
        };
        let ready_path = env::var(HELPER_READY_PATH)?;
        let release_path = env::var(HELPER_RELEASE_PATH)?;
        let mut events = Vec::new();
        let _lock = MachineLock::acquire("helper", Path::new(&lock_path), &mut events)?;
        fs::write(ready_path, b"ready")?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while !Path::new(&release_path).exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    fn acquire_contended_lock_times_out_with_one_notice() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let lock_path = directory.path().join("lock");
        let ready_path = directory.path().join("ready");
        let release_path = directory.path().join("release");
        let mut child = spawn_lock_holder(&lock_path, &ready_path, &release_path)?;
        wait_for_file(&ready_path)?;

        let timing = LockTiming {
            wait_notice_after: Duration::from_millis(20),
            timeout: Duration::from_millis(80),
            poll_interval: Duration::from_millis(2),
        };
        let mut events = Vec::new();
        let started = Instant::now();
        let error =
            match MachineLock::acquire_with_timing("ubuntu", &lock_path, &mut events, timing) {
                Err(error) => error,
                Ok(_) => panic!("the helper must hold the lock through the timeout"),
            };

        assert_eq!(error.kind(), ErrorKind::Busy);
        assert!(started.elapsed() >= timing.timeout);
        assert_eq!(
            events,
            vec![Event::Log {
                level: Level::Info,
                message: "waiting for another firestone operation on `ubuntu`".to_owned(),
            }]
        );

        fs::write(release_path, b"release")?;
        assert!(child.wait()?.success());
        Ok(())
    }

    #[test]
    fn acquire_contended_lock_after_release_succeeds_with_one_notice()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let lock_path = directory.path().join("lock");
        let ready_path = directory.path().join("ready");
        let release_path = directory.path().join("release");
        let mut child = spawn_lock_holder(&lock_path, &ready_path, &release_path)?;
        wait_for_file(&ready_path)?;

        let release_for_thread = release_path.clone();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            fs::write(release_for_thread, b"release")
        });
        let timing = LockTiming {
            wait_notice_after: Duration::from_millis(20),
            timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(2),
        };
        let mut events = Vec::new();
        let lock = MachineLock::acquire_with_timing("ubuntu", &lock_path, &mut events, timing)?;

        match releaser.join() {
            Ok(result) => result?,
            Err(_) => return Err("the release thread panicked".into()),
        }
        assert!(child.wait()?.success());
        assert_eq!(lock.path(), lock_path);
        assert_eq!(events.len(), 1);
        drop(lock);
        Ok(())
    }

    fn spawn_lock_holder(
        lock_path: &Path,
        ready_path: &Path,
        release_path: &Path,
    ) -> Result<std::process::Child, Box<dyn std::error::Error>> {
        let executable = env::current_exe()?;
        let child = Command::new(executable)
            .arg("--exact")
            .arg("lock::tests::lock_helper_process")
            .arg("--nocapture")
            .env(HELPER_LOCK_PATH, lock_path)
            .env(HELPER_READY_PATH, ready_path)
            .env(HELPER_RELEASE_PATH, release_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(child)
    }

    fn wait_for_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if path.exists() {
            Ok(())
        } else {
            Err(format!("lock helper did not create {}", path.display()).into())
        }
    }
}
