//! The only module of `firestone-init` that contains `unsafe` code.
//!
//! SPEC §10.5 unsafe policy. `nix` has safe wrappers for everything else this
//! program does — `mount`, `sethostname`, `waitpid`, `kill`, `setuid`,
//! `statvfs`, `reboot`, `socket`, `setsockopt`, `recv` — so unsafe is confined
//! to the three things it does not cover:
//!
//! 1. `EXT4_IOC_RESIZE_FS` and `BLKGETSIZE64`, needed to grow the root
//!    filesystem online without a `resize2fs` binary in the guest;
//! 2. the `SIOCxIF*` and `SIOCADDRT` interface ioctls, the only address
//!    configuration path that does not require a netlink implementation;
//! 3. `sendto` with a `sockaddr_ll`, because `AF_PACKET` link addresses have no
//!    safe constructor in `nix`.
//!
//! Every function below is a safe wrapper: it owns the whole lifetime of the
//! structures the kernel reads, borrows the descriptor it operates on, and
//! returns `Errno` instead of a raw return code. Every `unsafe` block carries a
//! `SAFETY:` comment naming the invariant it relies on. The crate denies
//! `unsafe_op_in_unsafe_fn`, so nothing here is implicitly unsafe.

use std::{
    ffi::CStr,
    mem,
    os::fd::{AsRawFd as _, BorrowedFd},
    ptr,
};

use nix::{errno::Errno, libc};

/// `struct rtentry` is laid out for a 64-bit `long` and pointer.
const _: () = assert!(
    mem::size_of::<usize>() == 8,
    "firestone-init targets 64-bit Linux guests only"
);
/// The IPv4 socket address must fit the generic one the ioctls take.
const _: () = assert!(mem::size_of::<libc::sockaddr_in>() <= mem::size_of::<libc::sockaddr>());

nix::ioctl_read!(blk_get_size64, 0x12, 114, u64);
nix::ioctl_write_ptr!(ext4_resize_fs, b'f', 16, u64);
nix::ioctl_readwrite_bad!(if_get_flags, libc::SIOCGIFFLAGS, libc::ifreq);
nix::ioctl_write_ptr_bad!(if_set_flags, libc::SIOCSIFFLAGS, libc::ifreq);
nix::ioctl_readwrite_bad!(if_get_index, libc::SIOCGIFINDEX, libc::ifreq);
nix::ioctl_write_ptr_bad!(if_set_address, libc::SIOCSIFADDR, libc::ifreq);
nix::ioctl_write_ptr_bad!(if_set_netmask, libc::SIOCSIFNETMASK, libc::ifreq);
nix::ioctl_write_ptr_bad!(route_add, libc::SIOCADDRT, RouteEntry);

/// `struct rtentry` from `include/uapi/linux/route.h`.
///
/// It is declared here rather than taken from `libc` because the type is not
/// exposed for every Linux target this crate compiles for, and the ioctl needs
/// one exact layout on the 64-bit guests Firestone runs.
#[repr(C)]
#[derive(Clone, Copy)]
struct RouteEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_tos: libc::c_uchar,
    rt_class: libc::c_uchar,
    rt_pad4: [libc::c_short; 3],
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

/// Reads the size in bytes of an opened block device (`BLKGETSIZE64`).
pub fn block_device_size(device: BorrowedFd<'_>) -> Result<u64, Errno> {
    let mut size = 0_u64;
    // SAFETY: `size` is a live, correctly aligned `u64` for the whole call, and
    // `BLKGETSIZE64` writes exactly one `u64` into it. The descriptor outlives
    // the call because it is borrowed.
    unsafe { blk_get_size64(device.as_raw_fd(), &raw mut size) }?;
    Ok(size)
}

/// Grows the mounted ext4 filesystem behind `mount_point` to `blocks`.
pub fn ext4_resize(mount_point: BorrowedFd<'_>, blocks: u64) -> Result<(), Errno> {
    // SAFETY: `EXT4_IOC_RESIZE_FS` reads one `u64` through the pointer and
    // never writes to it; `blocks` is a live local for the whole call.
    unsafe { ext4_resize_fs(mount_point.as_raw_fd(), &raw const blocks) }?;
    Ok(())
}

/// Returns the kernel interface index of `name`.
pub fn interface_index(socket: BorrowedFd<'_>, name: &str) -> Result<i32, Errno> {
    let mut request = interface_request(name)?;
    // SAFETY: `request` is a live, zero-initialized `ifreq` whose name field is
    // NUL-terminated; `SIOCGIFINDEX` reads that name and writes only the
    // `ifru_ifindex` member of the union.
    unsafe { if_get_index(socket.as_raw_fd(), &raw mut request) }?;
    // SAFETY: the ioctl above succeeded, so the kernel initialized the
    // `ifru_ifindex` member of the union; reading any other member would be
    // the unsound one.
    Ok(unsafe { request.ifr_ifru.ifru_ifindex })
}

/// Sets `IFF_UP` on one interface, leaving its other flags alone.
pub fn interface_bring_up(socket: BorrowedFd<'_>, name: &str) -> Result<(), Errno> {
    let mut request = interface_request(name)?;
    // SAFETY: as above; `SIOCGIFFLAGS` writes only `ifru_flags`.
    unsafe { if_get_flags(socket.as_raw_fd(), &raw mut request) }?;
    // SAFETY: the ioctl above initialized `ifru_flags`.
    let flags = unsafe { request.ifr_ifru.ifru_flags };
    request.ifr_ifru.ifru_flags = flags | (libc::IFF_UP as libc::c_short);
    // SAFETY: `SIOCSIFFLAGS` reads the name and `ifru_flags`, both of which are
    // initialized, and writes nothing.
    unsafe { if_set_flags(socket.as_raw_fd(), &raw const request) }?;
    Ok(())
}

/// Assigns one IPv4 address and netmask to an interface.
pub fn set_interface_ipv4(
    socket: BorrowedFd<'_>,
    name: &str,
    address: [u8; 4],
    netmask: [u8; 4],
) -> Result<(), Errno> {
    let mut request = interface_request(name)?;
    request.ifr_ifru.ifru_addr = socket_address_ipv4(address);
    // SAFETY: `SIOCSIFADDR` reads the name and the `ifru_addr` member, which
    // was just initialized with a complete `sockaddr_in`.
    unsafe { if_set_address(socket.as_raw_fd(), &raw const request) }?;

    let mut request = interface_request(name)?;
    request.ifr_ifru.ifru_netmask = socket_address_ipv4(netmask);
    // SAFETY: the same, for the netmask member.
    unsafe { if_set_netmask(socket.as_raw_fd(), &raw const request) }?;
    Ok(())
}

/// Installs `0.0.0.0/0 via gateway dev device` (`SIOCADDRT`).
pub fn add_default_route(
    socket: BorrowedFd<'_>,
    gateway: [u8; 4],
    device: &CStr,
) -> Result<(), Errno> {
    let mut device = device.to_owned().into_bytes_with_nul();
    // SAFETY: `RouteEntry` is a plain-old-data `repr(C)` struct with no
    // padding requirements beyond alignment and no invalid bit patterns, so an
    // all-zero value is a valid one.
    let mut route: RouteEntry = unsafe { mem::zeroed() };
    route.rt_dst = socket_address_ipv4([0, 0, 0, 0]);
    route.rt_genmask = socket_address_ipv4([0, 0, 0, 0]);
    route.rt_gateway = socket_address_ipv4(gateway);
    route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    route.rt_dev = device.as_mut_ptr().cast::<libc::c_char>();
    // SAFETY: `SIOCADDRT` reads the structure and the NUL-terminated device
    // name it points at. `device` is a live local that outlives the call, and
    // the kernel copies both before returning.
    unsafe { route_add(socket.as_raw_fd(), &raw const route) }?;
    drop(device);
    Ok(())
}

/// Sends one link-layer broadcast frame body on an `AF_PACKET`/`SOCK_DGRAM`
/// socket, which supplies the Ethernet header itself.
pub fn send_link_broadcast(
    socket: BorrowedFd<'_>,
    interface_index: i32,
    protocol: u16,
    payload: &[u8],
) -> Result<usize, Errno> {
    // SAFETY: `sockaddr_ll` is plain-old-data; an all-zero value is valid and
    // every field used below is written before the call.
    let mut address: libc::sockaddr_ll = unsafe { mem::zeroed() };
    address.sll_family = libc::AF_PACKET as libc::c_ushort;
    address.sll_protocol = protocol.to_be();
    address.sll_ifindex = interface_index;
    address.sll_halen = 6;
    address.sll_addr[..6].copy_from_slice(&[0xff_u8; 6]);
    // SAFETY: `payload` is a live slice for the duration of the call and its
    // length is passed exactly; `address` is a live, fully initialized
    // `sockaddr_ll` whose declared length matches its type. `sendto` reads both
    // and writes neither.
    let sent = unsafe {
        libc::sendto(
            socket.as_raw_fd(),
            payload.as_ptr().cast::<libc::c_void>(),
            payload.len(),
            0,
            ptr::from_ref(&address).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(Errno::last());
    }
    Ok(sent as usize)
}

fn interface_request(name: &str) -> Result<libc::ifreq, Errno> {
    if name.is_empty() || name.len() >= libc::IFNAMSIZ {
        return Err(Errno::EINVAL);
    }
    // SAFETY: `ifreq` is plain-old-data; an all-zero value is a valid one and
    // leaves the name NUL-terminated.
    let mut request: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in request.ifr_name.iter_mut().zip(name.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    Ok(request)
}

fn socket_address_ipv4(address: [u8; 4]) -> libc::sockaddr {
    let inet = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(address),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: `sockaddr` is plain-old-data, so an all-zero value is valid.
    let mut generic: libc::sockaddr = unsafe { mem::zeroed() };
    // SAFETY: both operands are live locals of plain-old-data types, the copy
    // length is `sockaddr_in`'s exact size, and the static assertion above
    // proves it is not larger than the `sockaddr` destination. The regions
    // cannot overlap because they are distinct locals.
    unsafe {
        ptr::copy_nonoverlapping(
            ptr::from_ref(&inet).cast::<u8>(),
            ptr::from_mut(&mut generic).cast::<u8>(),
            mem::size_of::<libc::sockaddr_in>(),
        );
    }
    generic
}
