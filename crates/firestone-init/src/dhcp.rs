//! A one-shot DHCP client codec, and the IPv4/UDP framing it rides on.
//!
//! SPEC §10.5: the pinned kernel's builtin `CONFIG_PACKET` is what lets PID 1
//! run its own client, because kernel `ip=dhcp` would spend about 176 s failing
//! on a machine with no server. `AF_PACKET`/`SOCK_DGRAM` hands the kernel the
//! link header but not the network one, so this module builds and parses the
//! IPv4 and UDP headers itself. Everything here is pure byte handling and is
//! covered by golden-byte tests on any host.

use std::fmt;

/// BOOTP fixed-area length, magic cookie included.
const FIXED_LEN: usize = 240;
/// Smallest BOOTP payload many servers accept.
const MIN_PACKET_LEN: usize = 300;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;
const FLAG_BROADCAST: u16 = 0x8000;

const OPTION_PAD: u8 = 0;
const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS: u8 = 6;
const OPTION_REQUESTED_IP: u8 = 50;
const OPTION_LEASE_TIME: u8 = 51;
const OPTION_MESSAGE_TYPE: u8 = 53;
const OPTION_SERVER_ID: u8 = 54;
const OPTION_PARAMETER_LIST: u8 = 55;
const OPTION_END: u8 = 255;

/// DHCP message types this client sends and understands.
pub const DHCP_DISCOVER: u8 = 1;
pub const DHCP_OFFER: u8 = 2;
pub const DHCP_REQUEST: u8 = 3;
pub const DHCP_ACK: u8 = 5;
pub const DHCP_NAK: u8 = 6;

/// The client and server UDP ports of RFC 2131.
pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

/// IPv4 protocol number for UDP.
const PROTO_UDP: u8 = 17;
const IPV4_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;

/// The lease facts a reply carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DhcpReply {
    pub message_type: u8,
    pub xid: u32,
    pub address: [u8; 4],
    pub subnet_mask: Option<[u8; 4]>,
    pub routers: Vec<[u8; 4]>,
    pub resolvers: Vec<[u8; 4]>,
    pub server_id: Option<[u8; 4]>,
    pub lease_seconds: Option<u32>,
}

/// Why a received datagram is not a usable DHCP reply.
#[derive(Debug, PartialEq, Eq)]
pub enum DhcpError {
    TooShort,
    NotAReply,
    BadCookie,
    NoMessageType,
}

impl fmt::Display for DhcpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooShort => "DHCP packet is shorter than the BOOTP fixed area",
            Self::NotAReply => "DHCP packet is not a BOOTP reply",
            Self::BadCookie => "DHCP packet has no magic cookie",
            Self::NoMessageType => "DHCP packet carries no message-type option",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for DhcpError {}

fn bootp_header(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0_u8; FIXED_LEN];
    packet[0] = OP_REQUEST;
    packet[1] = HTYPE_ETHERNET;
    packet[2] = HLEN_ETHERNET;
    packet[4..8].copy_from_slice(&xid.to_be_bytes());
    packet[10..12].copy_from_slice(&FLAG_BROADCAST.to_be_bytes());
    packet[28..34].copy_from_slice(&mac);
    packet[236..240].copy_from_slice(&MAGIC_COOKIE);
    packet
}

fn finish(mut packet: Vec<u8>) -> Vec<u8> {
    packet.push(OPTION_END);
    if packet.len() < MIN_PACKET_LEN {
        packet.resize(MIN_PACKET_LEN, OPTION_PAD);
    }
    packet
}

/// Builds a DHCPDISCOVER for one interface.
#[must_use]
pub fn encode_discover(xid: u32, mac: [u8; 6]) -> Vec<u8> {
    let mut packet = bootp_header(xid, mac);
    packet.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, DHCP_DISCOVER]);
    packet.extend_from_slice(&[
        OPTION_PARAMETER_LIST,
        3,
        OPTION_SUBNET_MASK,
        OPTION_ROUTER,
        OPTION_DNS,
    ]);
    finish(packet)
}

/// Builds the DHCPREQUEST that accepts one offer.
#[must_use]
pub fn encode_request(xid: u32, mac: [u8; 6], requested: [u8; 4], server_id: [u8; 4]) -> Vec<u8> {
    let mut packet = bootp_header(xid, mac);
    packet.extend_from_slice(&[OPTION_MESSAGE_TYPE, 1, DHCP_REQUEST]);
    packet.extend_from_slice(&[OPTION_SERVER_ID, 4]);
    packet.extend_from_slice(&server_id);
    packet.extend_from_slice(&[OPTION_REQUESTED_IP, 4]);
    packet.extend_from_slice(&requested);
    packet.extend_from_slice(&[
        OPTION_PARAMETER_LIST,
        3,
        OPTION_SUBNET_MASK,
        OPTION_ROUTER,
        OPTION_DNS,
    ]);
    finish(packet)
}

/// Parses one BOOTP reply into the lease facts the boot sequence applies.
pub fn parse_reply(packet: &[u8]) -> Result<DhcpReply, DhcpError> {
    if packet.len() < FIXED_LEN {
        return Err(DhcpError::TooShort);
    }
    if packet[0] != OP_REPLY {
        return Err(DhcpError::NotAReply);
    }
    if packet[236..240] != MAGIC_COOKIE {
        return Err(DhcpError::BadCookie);
    }
    let mut reply = DhcpReply {
        xid: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        address: [packet[16], packet[17], packet[18], packet[19]],
        ..DhcpReply::default()
    };
    let mut message_type = None;
    let mut index = FIXED_LEN;
    while index < packet.len() {
        let code = packet[index];
        index += 1;
        if code == OPTION_PAD {
            continue;
        }
        if code == OPTION_END {
            break;
        }
        let Some(&length) = packet.get(index) else {
            break;
        };
        index += 1;
        let length = usize::from(length);
        let Some(value) = packet.get(index..index + length) else {
            break;
        };
        index += length;
        match code {
            OPTION_MESSAGE_TYPE => message_type = value.first().copied(),
            OPTION_SUBNET_MASK => reply.subnet_mask = ipv4(value),
            OPTION_ROUTER => reply.routers = ipv4_list(value),
            OPTION_DNS => reply.resolvers = ipv4_list(value),
            OPTION_SERVER_ID => reply.server_id = ipv4(value),
            OPTION_LEASE_TIME => {
                reply.lease_seconds = value
                    .get(..4)
                    .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
            }
            _ => {}
        }
    }
    reply.message_type = message_type.ok_or(DhcpError::NoMessageType)?;
    Ok(reply)
}

fn ipv4(value: &[u8]) -> Option<[u8; 4]> {
    value
        .get(..4)
        .map(|bytes| [bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn ipv4_list(value: &[u8]) -> Vec<[u8; 4]> {
    value
        .chunks_exact(4)
        .map(|bytes| [bytes[0], bytes[1], bytes[2], bytes[3]])
        .collect()
}

/// One decoded IPv4/UDP datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    pub source: [u8; 4],
    pub destination_port: u16,
    pub payload: &'a [u8],
}

/// Wraps a payload in the IPv4 and UDP headers `AF_PACKET`/`SOCK_DGRAM` omits.
///
/// The UDP checksum is left zero, which RFC 768 allows over IPv4 and which
/// every DHCP server accepts; the IPv4 header checksum is always computed.
#[must_use]
pub fn ipv4_udp_datagram(
    source: [u8; 4],
    destination: [u8; 4],
    source_port: u16,
    destination_port: u16,
    identification: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total = IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
    let mut datagram = Vec::with_capacity(total);
    datagram.push(0x45);
    datagram.push(0x00);
    datagram.extend_from_slice(&(total as u16).to_be_bytes());
    datagram.extend_from_slice(&identification.to_be_bytes());
    datagram.extend_from_slice(&0_u16.to_be_bytes());
    datagram.push(64);
    datagram.push(PROTO_UDP);
    datagram.extend_from_slice(&0_u16.to_be_bytes());
    datagram.extend_from_slice(&source);
    datagram.extend_from_slice(&destination);
    let checksum = ones_complement_checksum(&datagram[..IPV4_HEADER_LEN]);
    datagram[10..12].copy_from_slice(&checksum.to_be_bytes());

    datagram.extend_from_slice(&source_port.to_be_bytes());
    datagram.extend_from_slice(&destination_port.to_be_bytes());
    datagram.extend_from_slice(&((UDP_HEADER_LEN + payload.len()) as u16).to_be_bytes());
    datagram.extend_from_slice(&0_u16.to_be_bytes());
    datagram.extend_from_slice(payload);
    datagram
}

/// Extracts the UDP payload of one received IPv4 datagram.
///
/// Fragments, non-UDP protocols and truncated headers return `None` rather than
/// an error: the client is reading a promiscuous device and simply skips what
/// is not its own traffic.
#[must_use]
pub fn parse_ipv4_udp(bytes: &[u8]) -> Option<UdpDatagram<'_>> {
    if bytes.len() < IPV4_HEADER_LEN || bytes[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(bytes[0] & 0x0f) * 4;
    if header_len < IPV4_HEADER_LEN || bytes.len() < header_len + UDP_HEADER_LEN {
        return None;
    }
    if bytes[9] != PROTO_UDP {
        return None;
    }
    let fragment = u16::from_be_bytes([bytes[6], bytes[7]]);
    if fragment & 0x1fff != 0 {
        return None;
    }
    let total_len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    let available = total_len.min(bytes.len());
    let udp = bytes.get(header_len..available)?;
    if udp.len() < UDP_HEADER_LEN {
        return None;
    }
    let udp_len = usize::from(u16::from_be_bytes([udp[4], udp[5]])).max(UDP_HEADER_LEN);
    let payload = udp.get(UDP_HEADER_LEN..udp_len.min(udp.len()))?;
    Some(UdpDatagram {
        source: [bytes[12], bytes[13], bytes[14], bytes[15]],
        destination_port: u16::from_be_bytes([udp[2], udp[3]]),
        payload,
    })
}

fn ones_complement_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0_u32;
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += u32::from(u16::from_be_bytes([last, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parses the `/sys/class/net/<dev>/address` contents into six bytes.
#[must_use]
pub fn parse_mac(value: &str) -> Option<[u8; 6]> {
    let mut mac = [0_u8; 6];
    let mut count = 0_usize;
    for field in value.trim().split(':') {
        if count == mac.len() {
            return None;
        }
        mac[count] = u8::from_str_radix(field, 16).ok()?;
        count += 1;
    }
    (count == mac.len()).then_some(mac)
}

/// Renders four bytes as dotted-quad text.
#[must_use]
pub fn format_ipv4(address: [u8; 4]) -> String {
    format!(
        "{}.{}.{}.{}",
        address[0], address[1], address[2], address[3]
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CLIENT_PORT, DHCP_ACK, DHCP_DISCOVER, DHCP_OFFER, DHCP_REQUEST, DhcpError, FIXED_LEN,
        MIN_PACKET_LEN, SERVER_PORT, encode_discover, encode_request, format_ipv4,
        ipv4_udp_datagram, parse_ipv4_udp, parse_mac, parse_reply,
    };

    const MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x9a, 0x1f, 0xc3];
    const XID: u32 = 0x1234_5678;

    fn reply(message_type: u8, options: &[u8]) -> Vec<u8> {
        let mut packet = vec![0_u8; FIXED_LEN];
        packet[0] = 2;
        packet[1] = 1;
        packet[2] = 6;
        packet[4..8].copy_from_slice(&XID.to_be_bytes());
        packet[16..20].copy_from_slice(&[192, 168, 1, 42]);
        packet[236..240].copy_from_slice(&[99, 130, 83, 99]);
        packet.extend_from_slice(&[53, 1, message_type]);
        packet.extend_from_slice(options);
        packet.push(255);
        packet
    }

    #[test]
    fn encode_discover_matches_the_golden_bytes() {
        let packet = encode_discover(XID, MAC);

        assert_eq!(packet.len(), MIN_PACKET_LEN);
        assert_eq!(&packet[..4], &[1, 1, 6, 0]);
        assert_eq!(&packet[4..8], &XID.to_be_bytes());
        assert_eq!(&packet[10..12], &[0x80, 0x00]);
        assert_eq!(&packet[28..34], &MAC);
        assert_eq!(&packet[236..240], &[99, 130, 83, 99]);
        assert_eq!(
            &packet[240..250],
            &[53, 1, DHCP_DISCOVER, 55, 3, 1, 3, 6, 255, 0]
        );
        assert!(packet[250..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn encode_request_carries_the_server_id_and_requested_address() {
        let packet = encode_request(XID, MAC, [192, 168, 1, 42], [192, 168, 1, 1]);

        assert_eq!(packet.len(), MIN_PACKET_LEN);
        assert_eq!(
            &packet[240..262],
            &[
                53,
                1,
                DHCP_REQUEST,
                54,
                4,
                192,
                168,
                1,
                1,
                50,
                4,
                192,
                168,
                1,
                42,
                55,
                3,
                1,
                3,
                6,
                255,
                0
            ]
        );
    }

    #[test]
    fn encode_is_byte_stable_for_equal_inputs() {
        assert_eq!(encode_discover(XID, MAC), encode_discover(XID, MAC));
    }

    #[test]
    fn parse_reply_offer_reads_every_option() -> Result<(), DhcpError> {
        let packet = reply(
            DHCP_OFFER,
            &[
                1, 4, 255, 255, 255, 0, // subnet mask
                3, 4, 192, 168, 1, 1, // router
                6, 8, 1, 1, 1, 1, 8, 8, 8, 8, // two resolvers
                54, 4, 192, 168, 1, 1, // server id
                51, 4, 0, 0, 14, 16, // lease
                0, 0, // padding between options
            ],
        );

        let parsed = parse_reply(&packet)?;

        assert_eq!(parsed.message_type, DHCP_OFFER);
        assert_eq!(parsed.xid, XID);
        assert_eq!(parsed.address, [192, 168, 1, 42]);
        assert_eq!(parsed.subnet_mask, Some([255, 255, 255, 0]));
        assert_eq!(parsed.routers, vec![[192, 168, 1, 1]]);
        assert_eq!(parsed.resolvers, vec![[1, 1, 1, 1], [8, 8, 8, 8]]);
        assert_eq!(parsed.server_id, Some([192, 168, 1, 1]));
        assert_eq!(parsed.lease_seconds, Some(3600));
        Ok(())
    }

    #[test]
    fn parse_reply_ack_without_options_keeps_the_address() -> Result<(), DhcpError> {
        let parsed = parse_reply(&reply(DHCP_ACK, &[]))?;

        assert_eq!(parsed.message_type, DHCP_ACK);
        assert_eq!(parsed.subnet_mask, None);
        assert!(parsed.routers.is_empty());
        Ok(())
    }

    #[test]
    fn parse_reply_short_packet_is_refused() {
        assert_eq!(parse_reply(&[0_u8; 10]), Err(DhcpError::TooShort));
    }

    #[test]
    fn parse_reply_request_opcode_is_refused() {
        let packet = encode_discover(XID, MAC);

        assert_eq!(parse_reply(&packet), Err(DhcpError::NotAReply));
    }

    #[test]
    fn parse_reply_wrong_cookie_is_refused() {
        let mut packet = reply(DHCP_OFFER, &[]);
        packet[236] = 0;

        assert_eq!(parse_reply(&packet), Err(DhcpError::BadCookie));
    }

    #[test]
    fn parse_reply_without_message_type_is_refused() {
        let mut packet = vec![0_u8; FIXED_LEN];
        packet[0] = 2;
        packet[236..240].copy_from_slice(&[99, 130, 83, 99]);
        packet.push(255);

        assert_eq!(parse_reply(&packet), Err(DhcpError::NoMessageType));
    }

    #[test]
    fn parse_reply_truncated_option_stops_without_panicking() {
        let mut packet = reply(DHCP_OFFER, &[]);
        packet.pop();
        packet.extend_from_slice(&[3, 4, 192, 168]);

        assert!(matches!(parse_reply(&packet), Ok(parsed) if parsed.routers.is_empty()));
    }

    #[test]
    fn ipv4_udp_datagram_matches_the_golden_header() {
        let datagram = ipv4_udp_datagram(
            [0, 0, 0, 0],
            [255, 255, 255, 255],
            CLIENT_PORT,
            SERVER_PORT,
            0,
            b"hi",
        );

        assert_eq!(
            datagram,
            vec![
                0x45, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x40, 0x11, 0x7a, 0xd0, 0x00, 0x00,
                0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x00, 0x44, 0x00, 0x43, 0x00, 0x0a, 0x00, 0x00,
                b'h', b'i',
            ]
        );
    }

    #[test]
    fn ipv4_udp_round_trip_returns_the_payload() {
        let datagram = ipv4_udp_datagram(
            [192, 168, 1, 1],
            [255, 255, 255, 255],
            SERVER_PORT,
            CLIENT_PORT,
            7,
            b"payload",
        );

        let parsed = parse_ipv4_udp(&datagram).ok_or("datagram must parse");

        match parsed {
            Ok(parsed) => {
                assert_eq!(parsed.source, [192, 168, 1, 1]);
                assert_eq!(parsed.destination_port, CLIENT_PORT);
                assert_eq!(parsed.payload, b"payload");
            }
            Err(reason) => panic!("{reason}"),
        }
    }

    #[test]
    fn parse_ipv4_udp_skips_other_protocols_and_fragments() {
        let mut datagram = ipv4_udp_datagram([1, 2, 3, 4], [5, 6, 7, 8], 1, 2, 0, b"x");
        let fragmented = {
            let mut clone = datagram.clone();
            clone[6] = 0x00;
            clone[7] = 0x01;
            clone
        };
        datagram[9] = 6;

        assert!(parse_ipv4_udp(&datagram).is_none());
        assert!(parse_ipv4_udp(&fragmented).is_none());
        assert!(parse_ipv4_udp(&[0x45, 0x00]).is_none());
    }

    #[test]
    fn parse_mac_reads_sysfs_form_and_refuses_others() {
        assert_eq!(parse_mac("52:54:00:9a:1f:c3\n"), Some(MAC));
        assert_eq!(parse_mac("52:54:00:9a:1f"), None);
        assert_eq!(parse_mac("52:54:00:9a:1f:c3:ff"), None);
        assert_eq!(parse_mac("not a mac"), None);
    }

    #[test]
    fn format_ipv4_renders_dotted_quad() {
        assert_eq!(format_ipv4([10, 0, 2, 15]), "10.0.2.15");
    }
}
