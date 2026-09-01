//! Host pseudo-terminal allocation for server-side interactive sessions.
//!
//! A browser terminal cannot speak the SSH wire protocol, so the WebSocket
//! shell transport runs OpenSSH on the host and gives it a real terminal:
//! the client's keystrokes become writes to the PTY master, the child's
//! output becomes WebSocket frames, and a browser resize becomes
//! `TIOCSWINSZ` on the master.
//!
//! Everything here is a safe `rustix` call. `unsafe_code` is forbidden
//! workspace-wide, and `openpt`/`grantpt`/`unlockpt`/`tcsetwinsize` all have
//! safe wrappers, so no `libc` call is written by hand.

use std::{
    ffi::OsString,
    io,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::{ffi::OsStringExt as _, fs::OpenOptionsExt as _},
    },
    path::PathBuf,
};

use rustix::{
    io::FdFlags,
    pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt},
    termios::{Winsize, tcsetwinsize},
};

use crate::{ErrorKind, FirestoneError};

/// One allocated host pseudo-terminal.
///
/// The master is the transport side and is always close-on-exec: a child that
/// inherited it would keep the master readable forever and the relay would
/// never observe the session ending. The slave is handed to the child as its
/// stdio and must be dropped by the parent right after the spawn, for the
/// same reason.
#[derive(Debug)]
pub struct PtyPair {
    master: OwnedFd,
    slave: OwnedFd,
    slave_path: PathBuf,
}

impl PtyPair {
    /// Allocates one PTY and unlocks its slave.
    ///
    /// The master is left blocking; call [`Self::set_master_nonblocking`] when
    /// the caller drives it from an async reactor.
    pub fn open() -> Result<Self, FirestoneError> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .map_err(|source| pty_error("allocate a host pseudo-terminal", source))?;
        rustix::io::fcntl_setfd(&master, FdFlags::CLOEXEC)
            .map_err(|source| pty_error("mark the pseudo-terminal master close-on-exec", source))?;
        grantpt(&master)
            .map_err(|source| pty_error("grant the pseudo-terminal slave device", source))?;
        unlockpt(&master)
            .map_err(|source| pty_error("unlock the pseudo-terminal slave device", source))?;
        let name = ptsname(&master, Vec::new())
            .map_err(|source| pty_error("resolve the pseudo-terminal slave path", source))?;
        let slave_path = PathBuf::from(OsString::from_vec(name.into_bytes()));
        // `O_NOCTTY` matters: without it, a session leader with no controlling
        // terminal — a daemonized `firestone serve` — would silently adopt a
        // machine's shell terminal as its own.
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY)
            .open(&slave_path)
            .map_err(|source| {
                FirestoneError::new(
                    ErrorKind::Dependency,
                    format!(
                        "cannot open the pseudo-terminal slave '{}'",
                        slave_path.display()
                    ),
                )
                .with_hint("check that /dev/pts is mounted and retry")
                .with_source(source)
            })?;
        Ok(Self {
            master,
            slave: OwnedFd::from(slave),
            slave_path,
        })
    }

    #[must_use]
    pub fn master(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    #[must_use]
    pub fn slave(&self) -> BorrowedFd<'_> {
        self.slave.as_fd()
    }

    /// The `/dev/pts/N` path the slave was opened from.
    #[must_use]
    pub fn slave_path(&self) -> &std::path::Path {
        &self.slave_path
    }

    /// Puts the master into non-blocking mode for an async reactor.
    pub fn set_master_nonblocking(&self) -> Result<(), FirestoneError> {
        rustix::io::ioctl_fionbio(&self.master, true)
            .map_err(|source| pty_error("make the pseudo-terminal master nonblocking", source))
    }

    /// Applies a client-requested geometry to the master.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), FirestoneError> {
        set_window_size(self.master.as_fd(), rows, cols)
    }

    /// Splits the pair into `(master, slave)` so each side can be moved on.
    #[must_use]
    pub fn into_parts(self) -> (OwnedFd, OwnedFd) {
        (self.master, self.slave)
    }
}

/// Applies `TIOCSWINSZ` to one terminal file descriptor.
///
/// Zero rows or columns are refused: a terminal with no cells is not a
/// geometry, and passing one through would make the guest's own resize
/// handling undefined.
pub fn set_window_size(fd: BorrowedFd<'_>, rows: u16, cols: u16) -> Result<(), FirestoneError> {
    if rows == 0 || cols == 0 {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("a terminal geometry must have non-zero rows and columns, got {rows}x{cols}"),
        )
        .with_hint("send {\"resize\":{\"rows\":24,\"cols\":80}} with positive dimensions"));
    }
    tcsetwinsize(
        fd,
        Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .map_err(|source| pty_error("apply the requested terminal size", source))
}

fn pty_error(phase: &str, source: rustix::io::Errno) -> FirestoneError {
    FirestoneError::new(ErrorKind::Dependency, format!("cannot {phase}"))
        .with_hint("check the host pseudo-terminal limits and retry")
        .with_source(io::Error::from_raw_os_error(source.raw_os_error()))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::{PtyPair, set_window_size};
    use crate::ErrorKind;

    #[test]
    fn open_pty_pair_returns_a_connected_master_and_slave() {
        let pair = PtyPair::open().expect("a host pseudo-terminal is allocatable");
        assert!(pair.slave_path().exists());
        let mut slave = std::fs::File::from(
            pair.slave()
                .try_clone_to_owned()
                .expect("the slave is duplicable"),
        );
        let mut master = std::fs::File::from(
            pair.master()
                .try_clone_to_owned()
                .expect("the master is duplicable"),
        );
        slave.write_all(b"hi").expect("the slave accepts a write");
        let mut buffer = [0_u8; 8];
        let read = master.read(&mut buffer).expect("the master sees the write");
        assert!(read >= 2, "{read}");
        assert_eq!(&buffer[..2], b"hi");
    }

    #[test]
    fn set_window_size_zero_dimension_is_rejected() {
        let pair = PtyPair::open().expect("a host pseudo-terminal is allocatable");
        let error = set_window_size(pair.master(), 0, 80).expect_err("zero rows are refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
        assert!(
            error
                .info()
                .hint
                .is_some_and(|hint| hint.contains("resize"))
        );
        let error = set_window_size(pair.master(), 24, 0).expect_err("zero columns are refused");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn resize_applies_the_requested_geometry_to_the_slave() {
        let pair = PtyPair::open().expect("a host pseudo-terminal is allocatable");
        pair.resize(24, 80).expect("a valid geometry is applied");
        let size = rustix::termios::tcgetwinsize(pair.slave()).expect("the slave reports a size");
        assert_eq!(size.ws_row, 24);
        assert_eq!(size.ws_col, 80);

        pair.resize(50, 132).expect("a resize is applied");
        let size = rustix::termios::tcgetwinsize(pair.slave()).expect("the slave reports a size");
        assert_eq!(size.ws_row, 50);
        assert_eq!(size.ws_col, 132);
    }

    #[test]
    fn master_nonblocking_read_without_data_reports_would_block() {
        let pair = PtyPair::open().expect("a host pseudo-terminal is allocatable");
        pair.set_master_nonblocking()
            .expect("the master accepts O_NONBLOCK");
        let mut master = std::fs::File::from(
            pair.master()
                .try_clone_to_owned()
                .expect("the master is duplicable"),
        );
        let mut buffer = [0_u8; 8];
        let error = master.read(&mut buffer).expect_err("no data is pending");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }
}
