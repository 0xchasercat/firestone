//! The one place `firestone-init` talks to a human.
//!
//! Everything the guest's PID 1 has to say goes to `/dev/console`, which
//! Cloud Hypervisor mirrors onto both `hvc0` and `console.log` (SPEC §9.5), so
//! `firestone logs` and `firestone console` show the same lines. Writing to the
//! console can itself fail; there is nowhere better to report that, so the
//! fallback is standard error and then silence.

use std::{
    fs::File,
    io::{self, Write},
};

const CONSOLE_DEVICE: &str = "/dev/console";
const PREFIX: &str = "firestone-init: ";

/// A best-effort line writer for the guest console.
#[derive(Debug)]
pub struct Console {
    device: Option<File>,
}

impl Console {
    /// Opens `/dev/console`, falling back to standard error.
    #[must_use]
    pub fn open() -> Self {
        Self {
            device: File::options().append(true).open(CONSOLE_DEVICE).ok(),
        }
    }

    /// Writes one prefixed line. Failures are unreportable and dropped.
    pub fn line(&mut self, message: &str) {
        let mut rendered = String::with_capacity(PREFIX.len() + message.len() + 1);
        rendered.push_str(PREFIX);
        rendered.push_str(message);
        rendered.push('\n');
        if let Some(device) = self.device.as_mut() {
            if device.write_all(rendered.as_bytes()).is_ok() {
                let _ = device.flush();
                return;
            }
        }
        let mut stderr = io::stderr();
        let _ = stderr.write_all(rendered.as_bytes());
        let _ = stderr.flush();
    }

    /// Writes one line prefixed with `warning:`; boot continues afterwards.
    pub fn warn(&mut self, message: &str) {
        self.line(&format!("warning: {message}"));
    }
}
