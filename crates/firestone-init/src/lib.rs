//! `firestone-init` — the guest PID 1 of a Firestone OCI machine (SPEC §10.5).
//!
//! An OCI guest has no cloud-init, no systemd and no sshd. It has this static
//! binary at `/sbin/firestone-init`, started by the pinned kernel through
//! `init=/sbin/firestone-init` (SPEC §9.5), and the image's entrypoint running
//! as its child. The program mounts the pseudo-filesystems, reads its
//! configuration from the config disk on `/dev/vdb`, grows the root filesystem,
//! configures networking, starts the entrypoint, and then stays PID 1: reaping
//! orphans, forwarding termination signals to the child's process group, and
//! powering the machine off when the child exits.
//!
//! The Linux runtime lives in [`runtime`], `net` and `ffi`; every decision that
//! can be made without a kernel lives in [`config`], [`dhcp`], [`exec`],
//! [`resize`] and [`users`], which compile and test on every host. That split
//! is what keeps the workspace gate green on macOS, exactly as `shim.rs` does.

pub mod config;
pub mod console;
pub mod dhcp;
pub mod exec;
pub mod resize;
pub mod users;

#[cfg(target_os = "linux")]
pub(crate) mod ffi;
#[cfg(target_os = "linux")]
pub mod net;
#[cfg(target_os = "linux")]
pub mod runtime;
