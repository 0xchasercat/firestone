//! The PID-1 boot sequence and supervision loop (SPEC §10.5).
//!
//! Everything in this module is Linux-only; the pure decision logic it calls
//! lives in sibling modules that compile everywhere, so the workspace gate stays
//! green on a macOS host exactly as it does for the shim.

use std::{
    fs::{self, File},
    os::fd::AsFd as _,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use firestone_initproto::{InitConfig, InitNetwork};
use nix::{
    errno::Errno,
    mount::{MsFlags, mount},
    sys::{
        reboot::{RebootMode, reboot},
        signal::{SigSet, Signal, kill},
        signalfd::{SfdFlags, SignalFd},
        statvfs::statvfs,
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, getpid, sethostname, sync},
};

use crate::{
    config::{CONFIG_DEVICE, read_config},
    console::Console,
    exec::{ChildPlan, plan_child},
    ffi, net,
    resize::resize_target_blocks,
    users::{ResolvedUser, resolve_user},
};

const CONSOLE_DEVICE: &str = "/dev/console";
const ROOT_DEVICE: &str = "/dev/vda";
const PASSWD_FILE: &str = "/etc/passwd";
const GROUP_FILE: &str = "/etc/group";

/// One pseudo-filesystem to mount during step 1.
struct PseudoMount {
    source: &'static str,
    target: &'static str,
    filesystem: &'static str,
    flags: MsFlags,
    data: Option<&'static str>,
}

const PSEUDO_MOUNTS: &[PseudoMount] = &[
    PseudoMount {
        source: "proc",
        target: "/proc",
        filesystem: "proc",
        flags: MsFlags::MS_NOSUID
            .union(MsFlags::MS_NODEV)
            .union(MsFlags::MS_NOEXEC),
        data: None,
    },
    PseudoMount {
        source: "sysfs",
        target: "/sys",
        filesystem: "sysfs",
        flags: MsFlags::MS_NOSUID
            .union(MsFlags::MS_NODEV)
            .union(MsFlags::MS_NOEXEC),
        data: None,
    },
    PseudoMount {
        source: "devtmpfs",
        target: "/dev",
        filesystem: "devtmpfs",
        flags: MsFlags::MS_NOSUID,
        data: Some("mode=0755"),
    },
    PseudoMount {
        source: "devpts",
        target: "/dev/pts",
        filesystem: "devpts",
        flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NOEXEC),
        data: Some("gid=5,mode=620,ptmxmode=666"),
    },
    PseudoMount {
        source: "tmpfs",
        target: "/run",
        filesystem: "tmpfs",
        flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV),
        data: Some("mode=0755"),
    },
    PseudoMount {
        source: "tmpfs",
        target: "/tmp",
        filesystem: "tmpfs",
        flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV),
        data: Some("mode=1777"),
    },
    PseudoMount {
        source: "tmpfs",
        target: "/dev/shm",
        filesystem: "tmpfs",
        flags: MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV),
        data: Some("mode=1777"),
    },
];

/// Runs the whole guest lifecycle. This function never returns.
pub fn run() -> ! {
    let mut console = Console::open();
    if getpid() != Pid::from_raw(1) {
        console.line("firestone-init runs as PID 1 inside a Firestone OCI machine only");
        std::process::exit(1);
    }

    mount_pseudo_filesystems(&mut console);

    let config = match read_config(Path::new(CONFIG_DEVICE)) {
        Ok(config) => config,
        Err(error) => {
            console.line(&format!("{error}"));
            power_off(&mut console);
        }
    };

    if let Err(error) = net::bring_loopback_up() {
        console.warn(&format!("{error}"));
    }
    grow_root_filesystem(&mut console, config.disk_size_bytes);
    if let Err(error) = sethostname(config.hostname.as_str()) {
        console.warn(&format!("cannot set the hostname: {error}"));
    }
    configure_network(&mut console, &config);

    supervise(&mut console, &config)
}

fn mount_pseudo_filesystems(console: &mut Console) {
    for entry in PSEUDO_MOUNTS {
        let _ = fs::create_dir_all(entry.target);
        let result = mount(
            Some(entry.source),
            entry.target,
            Some(entry.filesystem),
            entry.flags,
            entry.data,
        );
        match result {
            // `EBUSY` means the kernel already mounted it — `CONFIG_DEVTMPFS_MOUNT`
            // does exactly that for `/dev` (SPEC §10.5 kernel facts).
            Ok(()) | Err(Errno::EBUSY) => {}
            Err(error) => console.warn(&format!(
                "cannot mount {} on {}: {error}",
                entry.filesystem, entry.target
            )),
        }
    }
}

fn grow_root_filesystem(console: &mut Console, requested_bytes: u64) {
    let stats = match statvfs("/") {
        Ok(stats) => stats,
        Err(error) => {
            console.warn(&format!("cannot inspect the root filesystem: {error}"));
            return;
        }
    };
    let block_size = stats.fragment_size();
    let current_blocks = stats.blocks();

    let device = match File::open(ROOT_DEVICE) {
        Ok(device) => device,
        Err(error) => {
            console.warn(&format!("cannot open {ROOT_DEVICE}: {error}"));
            return;
        }
    };
    let device_bytes = match ffi::block_device_size(device.as_fd()) {
        Ok(size) => size,
        Err(error) => {
            console.warn(&format!("cannot read the size of {ROOT_DEVICE}: {error}"));
            return;
        }
    };

    let Some(target) =
        resize_target_blocks(requested_bytes, device_bytes, block_size, current_blocks)
    else {
        return;
    };
    let root = match File::open("/") {
        Ok(root) => root,
        Err(error) => {
            console.warn(&format!("cannot open the root directory: {error}"));
            return;
        }
    };
    match ffi::ext4_resize(root.as_fd(), target) {
        Ok(()) => console.line(&format!(
            "root filesystem grown to {} blocks of {block_size} bytes",
            target
        )),
        Err(error) => console.warn(&format!("cannot grow the root filesystem: {error}")),
    }
}

fn configure_network(console: &mut Console, config: &InitConfig) {
    if config.network == InitNetwork::None {
        if let Err(error) = net::write_static_hosts(&config.hostname) {
            console.warn(&format!("{error}"));
        }
        return;
    }
    let mac = match net::interface_mac() {
        Ok(mac) => mac,
        Err(error) => {
            console.warn(&format!("{error}"));
            return;
        }
    };
    match net::acquire_lease(mac, net::DHCP_BUDGET) {
        Ok(lease) => {
            if let Err(error) = net::apply_lease(&lease, &config.hostname) {
                console.warn(&format!("{error}"));
                return;
            }
            console.line(&format!(
                "{} configured with {}",
                net::INTERFACE,
                crate::dhcp::format_ipv4(lease.address)
            ));
        }
        Err(error) => {
            console.warn(&format!("{error}"));
            if let Err(error) = net::write_static_hosts(&config.hostname) {
                console.warn(&format!("{error}"));
            }
        }
    }
}

fn supervise(console: &mut Console, config: &InitConfig) -> ! {
    let plan = match plan_child(config) {
        Ok(plan) => plan,
        Err(error) => {
            console.line(&format!("{error}"));
            power_off(console);
        }
    };
    let user = match resolve_account(config.user.as_deref()) {
        Ok(user) => user,
        Err(message) => {
            console.line(&message);
            power_off(console);
        }
    };

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGCHLD);
    if let Err(error) = mask.thread_block() {
        console.warn(&format!("cannot block supervision signals: {error}"));
    }
    let signals = match SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC) {
        Ok(signals) => signals,
        Err(error) => {
            console.line(&format!("cannot open the supervision signalfd: {error}"));
            power_off(console);
        }
    };

    let child = match spawn_child(&plan, &user) {
        Ok(child) => child,
        Err(error) => {
            console.line(&format!("cannot start `{}`: {error}", plan.program));
            power_off(console);
        }
    };
    let child_pid = Pid::from_raw(child as i32);
    console.line(&format!("started `{}` as pid {child}", plan.program));

    loop {
        match signals.read_signal() {
            Ok(Some(info)) => match Signal::try_from(info.ssi_signo as i32) {
                Ok(Signal::SIGCHLD) => {
                    if let Some(status) = reap(child_pid) {
                        report_exit(console, status);
                        power_off(console);
                    }
                }
                Ok(signal @ (Signal::SIGTERM | Signal::SIGINT)) => {
                    // Forward to the child's own process group, never to -1,
                    // so PID 1 does not signal itself (SPEC §10.5).
                    let _ = kill(Pid::from_raw(-child_pid.as_raw()), signal);
                }
                _ => {}
            },
            Ok(None) => {}
            Err(Errno::EINTR) => {}
            Err(error) => {
                console.warn(&format!("supervision read failed: {error}"));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn resolve_account(user: Option<&str>) -> Result<ResolvedUser, String> {
    let Some(user) = user.map(str::trim).filter(|user| !user.is_empty()) else {
        return Ok(ResolvedUser::root());
    };
    let passwd = fs::read_to_string(PASSWD_FILE).unwrap_or_default();
    let group = fs::read_to_string(GROUP_FILE).unwrap_or_default();
    resolve_user(user, &passwd, &group).map_err(|error| error.to_string())
}

fn spawn_child(plan: &ChildPlan, user: &ResolvedUser) -> Result<u32, std::io::Error> {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .env_clear()
        .envs(plan.env.iter().map(|(key, value)| (key, value)))
        .stdin(console_stdio()?)
        .stdout(console_stdio()?)
        .stderr(console_stdio()?)
        .process_group(0)
        .gid(user.gid)
        .uid(user.uid);
    if let Some(workdir) = &plan.workdir {
        command.current_dir(workdir);
    }
    if let Some(home) = &user.home {
        if !plan.env.iter().any(|(key, _)| key == "HOME") {
            command.env("HOME", home);
        }
    }
    command.spawn().map(|child| child.id())
}

fn console_stdio() -> Result<Stdio, std::io::Error> {
    File::options()
        .read(true)
        .write(true)
        .open(CONSOLE_DEVICE)
        .map(Stdio::from)
}

/// Reaps every finished process, returning the direct child's status if it is
/// among them. This is the whole of PID 1's orphan duty.
fn reap(child: Pid) -> Option<WaitStatus> {
    let mut finished = None;
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => return finished,
            Ok(status) => {
                if status.pid() == Some(child) {
                    finished = Some(status);
                }
            }
            Err(Errno::EINTR) => {}
            Err(_) => return finished,
        }
    }
}

fn report_exit(console: &mut Console, status: WaitStatus) {
    match status {
        WaitStatus::Exited(_, code) => {
            console.line(&format!("entrypoint exited with status {code}"))
        }
        WaitStatus::Signaled(_, signal, _) => {
            console.line(&format!("entrypoint was killed by {signal}"));
        }
        other => console.line(&format!("entrypoint finished: {other:?}")),
    }
}

/// Flushes and powers the machine off so Cloud Hypervisor exits cleanly.
fn power_off(console: &mut Console) -> ! {
    sync();
    match reboot(RebootMode::RB_POWER_OFF) {
        Ok(_) => {}
        Err(error) => console.line(&format!("cannot power off: {error}")),
    }
    // `reboot` only returns on failure. PID 1 may not exit — that panics the
    // kernel — so park instead and let the host stop the machine.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
