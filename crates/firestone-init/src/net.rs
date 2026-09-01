//! Bringing guest networking up: loopback, the one-shot DHCP client, and the
//! files a userland expects to find afterwards.
//!
//! SPEC §10.5 step 5. The whole exchange is bounded: one DISCOVER, one REQUEST,
//! and a total budget measured in seconds. A timeout prints one warning and
//! boot continues, because a machine whose network never came up is still a
//! machine the operator can reach on the console.

use std::{
    ffi::OsString,
    fmt, fs, io,
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd},
    path::Path,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    sys::{
        socket::{
            AddressFamily, MsgFlags, SockFlag, SockProtocol, SockType, recv, setsockopt, socket,
            sockopt,
        },
        time::{TimeVal, TimeValLike as _},
    },
};

use crate::{
    dhcp::{
        self, CLIENT_PORT, DHCP_ACK, DHCP_NAK, DHCP_OFFER, SERVER_PORT, format_ipv4,
        ipv4_udp_datagram, parse_ipv4_udp, parse_reply,
    },
    ffi,
};

/// The guest NIC Cloud Hypervisor always presents first.
pub const INTERFACE: &str = "eth0";
/// Total wall-clock budget for the whole DHCP exchange (SPEC §10.5).
pub const DHCP_BUDGET: Duration = Duration::from_secs(5);
/// How long one receive may block before the budget is re-checked.
const RECEIVE_SLICE: Duration = Duration::from_millis(500);
/// `ETH_P_IP`.
const ETHERNET_PROTOCOL_IPV4: u16 = 0x0800;
const RESOLV_CONF: &str = "/etc/resolv.conf";
const HOSTS: &str = "/etc/hosts";
const MAX_DATAGRAM: usize = 2048;

/// The address facts one completed lease produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub address: [u8; 4],
    pub netmask: [u8; 4],
    pub gateway: Option<[u8; 4]>,
    pub resolvers: Vec<[u8; 4]>,
}

/// Why guest networking could not be configured.
#[derive(Debug)]
pub enum NetError {
    Syscall {
        operation: &'static str,
        source: Errno,
    },
    Timeout,
    Refused,
    Io {
        path: String,
        source: io::Error,
    },
}

impl fmt::Display for NetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syscall { operation, source } => {
                write!(formatter, "{operation} failed: {source}")
            }
            Self::Timeout => write!(
                formatter,
                "no DHCP reply on {INTERFACE} within {} seconds",
                DHCP_BUDGET.as_secs()
            ),
            Self::Refused => write!(formatter, "the DHCP server refused the lease (NAK)"),
            Self::Io { path, source } => write!(formatter, "cannot write {path}: {source}"),
        }
    }
}

impl std::error::Error for NetError {}

fn syscall(operation: &'static str) -> impl FnOnce(Errno) -> NetError {
    move |source| NetError::Syscall { operation, source }
}

fn control_socket() -> Result<OwnedFd, NetError> {
    socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(syscall("open an AF_INET control socket"))
}

/// Brings `lo` up so a guest's own services can reach 127.0.0.1.
pub fn bring_loopback_up() -> Result<(), NetError> {
    let control = control_socket()?;
    ffi::interface_bring_up(control.as_fd(), "lo").map_err(syscall("bring lo up"))
}

/// Reads the MAC of `INTERFACE` from sysfs.
pub fn interface_mac() -> Result<[u8; 6], NetError> {
    let path = format!("/sys/class/net/{INTERFACE}/address");
    let contents = fs::read_to_string(&path).map_err(|source| NetError::Io { path, source })?;
    dhcp::parse_mac(&contents).ok_or(NetError::Syscall {
        operation: "parse the eth0 hardware address",
        source: Errno::EINVAL,
    })
}

/// Runs one bounded DHCP exchange on `INTERFACE` and returns the lease.
pub fn acquire_lease(mac: [u8; 6], budget: Duration) -> Result<Lease, NetError> {
    let deadline = Instant::now() + budget;
    let control = control_socket()?;
    ffi::interface_bring_up(control.as_fd(), INTERFACE).map_err(syscall("bring eth0 up"))?;
    let index =
        ffi::interface_index(control.as_fd(), INTERFACE).map_err(syscall("read the eth0 index"))?;

    let packet = socket(
        AddressFamily::Packet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::EthAll,
    )
    .map_err(syscall("open an AF_PACKET socket"))?;
    setsockopt(&packet, sockopt::BindToDevice, &OsString::from(INTERFACE))
        .map_err(syscall("bind the AF_PACKET socket to eth0"))?;
    setsockopt(
        &packet,
        sockopt::ReceiveTimeout,
        &TimeVal::milliseconds(RECEIVE_SLICE.as_millis() as i64),
    )
    .map_err(syscall("set the DHCP receive timeout"))?;

    let xid = transaction_id(mac);
    send(&packet, index, &dhcp::encode_discover(xid, mac))?;
    let offer = wait_for(&packet, xid, deadline, DHCP_OFFER)?;
    let server = offer.server_id.unwrap_or(offer.address);
    send(
        &packet,
        index,
        &dhcp::encode_request(xid, mac, offer.address, server),
    )?;
    let ack = wait_for(&packet, xid, deadline, DHCP_ACK)?;

    let lease = Lease {
        address: ack.address,
        netmask: ack
            .subnet_mask
            .or(offer.subnet_mask)
            .unwrap_or([255, 255, 255, 0]),
        gateway: ack
            .routers
            .first()
            .copied()
            .or_else(|| offer.routers.first().copied()),
        resolvers: if ack.resolvers.is_empty() {
            offer.resolvers.clone()
        } else {
            ack.resolvers.clone()
        },
    };
    Ok(lease)
}

fn send(socket: &OwnedFd, index: i32, payload: &[u8]) -> Result<(), NetError> {
    let datagram = ipv4_udp_datagram(
        [0, 0, 0, 0],
        [255, 255, 255, 255],
        CLIENT_PORT,
        SERVER_PORT,
        0,
        payload,
    );
    ffi::send_link_broadcast(socket.as_fd(), index, ETHERNET_PROTOCOL_IPV4, &datagram)
        .map_err(syscall("send a DHCP packet"))?;
    Ok(())
}

fn wait_for(
    socket: &OwnedFd,
    xid: u32,
    deadline: Instant,
    expected: u8,
) -> Result<dhcp::DhcpReply, NetError> {
    let mut buffer = vec![0_u8; MAX_DATAGRAM];
    while Instant::now() < deadline {
        let read = match recv(socket.as_raw_fd(), &mut buffer, MsgFlags::empty()) {
            Ok(read) => read,
            Err(Errno::EAGAIN | Errno::EINTR) => continue,
            Err(source) => return Err(syscall("receive a DHCP packet")(source)),
        };
        let Some(datagram) = parse_ipv4_udp(&buffer[..read]) else {
            continue;
        };
        if datagram.destination_port != CLIENT_PORT {
            continue;
        }
        let Ok(reply) = parse_reply(datagram.payload) else {
            continue;
        };
        if reply.xid != xid {
            continue;
        }
        if reply.message_type == DHCP_NAK {
            return Err(NetError::Refused);
        }
        if reply.message_type == expected {
            return Ok(reply);
        }
    }
    Err(NetError::Timeout)
}

/// A transaction id that differs between boots without a random source.
fn transaction_id(mac: [u8; 6]) -> u32 {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let mac_part = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    seed ^ mac_part.rotate_left(7) ^ 0x4653_544e
}

/// Applies one lease to `eth0` and writes the resolver files.
pub fn apply_lease(lease: &Lease, hostname: &str) -> Result<(), NetError> {
    let control = control_socket()?;
    ffi::set_interface_ipv4(control.as_fd(), INTERFACE, lease.address, lease.netmask)
        .map_err(syscall("assign the eth0 address"))?;
    if let Some(gateway) = lease.gateway {
        ffi::add_default_route(control.as_fd(), gateway, c"eth0")
            .map_err(syscall("add the default route"))?;
    }
    write_if_absent(
        Path::new(RESOLV_CONF),
        &render_resolv_conf(&lease.resolvers),
    )?;
    write_if_absent(Path::new(HOSTS), &render_hosts(lease.address, hostname))?;
    Ok(())
}

/// Writes the resolver and hosts files for a machine with no lease.
pub fn write_static_hosts(hostname: &str) -> Result<(), NetError> {
    write_if_absent(Path::new(HOSTS), &render_hosts([127, 0, 0, 1], hostname))
}

fn write_if_absent(path: &Path, contents: &str) -> Result<(), NetError> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, contents).map_err(|source| NetError::Io {
        path: path.display().to_string(),
        source,
    })
}

/// Renders `/etc/resolv.conf` for one resolver list.
#[must_use]
pub fn render_resolv_conf(resolvers: &[[u8; 4]]) -> String {
    let mut rendered = String::new();
    for resolver in resolvers {
        rendered.push_str("nameserver ");
        rendered.push_str(&format_ipv4(*resolver));
        rendered.push('\n');
    }
    rendered
}

/// Renders `/etc/hosts` for one address and hostname.
#[must_use]
pub fn render_hosts(address: [u8; 4], hostname: &str) -> String {
    let mut rendered = String::from("127.0.0.1\tlocalhost\n");
    if address != [127, 0, 0, 1] {
        rendered.push_str(&format_ipv4(address));
        rendered.push('\t');
        rendered.push_str(hostname);
        rendered.push('\n');
    } else {
        rendered.push_str("127.0.1.1\t");
        rendered.push_str(hostname);
        rendered.push('\n');
    }
    rendered.push_str("::1\tlocalhost ip6-localhost ip6-loopback\n");
    rendered
}

#[cfg(test)]
mod tests {
    use super::{render_hosts, render_resolv_conf};

    #[test]
    fn render_resolv_conf_writes_one_line_per_resolver() {
        assert_eq!(
            render_resolv_conf(&[[1, 1, 1, 1], [8, 8, 8, 8]]),
            "nameserver 1.1.1.1\nnameserver 8.8.8.8\n"
        );
    }

    #[test]
    fn render_resolv_conf_without_resolvers_is_empty() {
        assert!(render_resolv_conf(&[]).is_empty());
    }

    #[test]
    fn render_hosts_maps_the_leased_address_to_the_hostname() {
        assert_eq!(
            render_hosts([10, 0, 2, 15], "app"),
            "127.0.0.1\tlocalhost\n10.0.2.15\tapp\n::1\tlocalhost ip6-localhost ip6-loopback\n"
        );
    }

    #[test]
    fn render_hosts_without_a_lease_uses_the_loopback_alias() {
        assert_eq!(
            render_hosts([127, 0, 0, 1], "app"),
            "127.0.0.1\tlocalhost\n127.0.1.1\tapp\n::1\tlocalhost ip6-localhost ip6-loopback\n"
        );
    }
}
