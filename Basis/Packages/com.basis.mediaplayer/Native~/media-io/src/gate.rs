//! The address gate: the one implementation of the SSRF blocklist (L13).
//! Policy above it (consent, per-world allowlists) lives managed-side; the
//! mechanism lives here and is consulted for every resolved address and
//! every redirect hop.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub trait AddressGate: Send + Sync {
    fn permit(&self, ip: IpAddr) -> bool;
}

/// Default gate: publicly routable addresses only.
pub struct PublicAddressGate;

impl AddressGate for PublicAddressGate {
    fn permit(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => permit_v4(v4),
            IpAddr::V6(v6) => permit_v6(v6),
        }
    }
}

fn permit_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        // CGNAT 100.64.0.0/10.
        || (a == 100 && (64..128).contains(&b))
        // IETF protocol assignments 192.0.0.0/24.
        || (a == 192 && b == 0 && ip.octets()[2] == 0)
        // Benchmarking 198.18.0.0/15.
        || (a == 198 && (b == 18 || b == 19))
        // Multicast + reserved 224.0.0.0/3.
        || a >= 224)
}

fn permit_v6(ip: Ipv6Addr) -> bool {
    // Addresses that embed a v4 address are gated on the embedded address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return permit_v4(v4);
    }
    let segments = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96.
    if segments[0] == 0x64 && segments[1] == 0xff9b {
        let [.., g, h] = segments;
        return permit_v4(Ipv4Addr::new(
            (g >> 8) as u8,
            g as u8,
            (h >> 8) as u8,
            h as u8,
        ));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        // Unique-local fc00::/7.
        || (segments[0] & 0xfe00) == 0xfc00
        // Link-local fe80::/10.
        || (segments[0] & 0xffc0) == 0xfe80
        // Multicast ff00::/8.
        || (segments[0] & 0xff00) == 0xff00
        // Documentation 2001:db8::/32.
        || (segments[0] == 0x2001 && segments[1] == 0xdb8))
}

/// Explicit opt-out for local fixtures and the test rig. Never a default.
pub struct AllowAllGate;

impl AddressGate for AllowAllGate {
    fn permit(&self, _ip: IpAddr) -> bool {
        true
    }
}

/// Resolve a bare host and vet every returned address (§9.3) — the
/// pre-connect check for transports whose clients do their own dialling
/// (RTSP). A mixed public/private answer is the rebinding shape and is
/// refused whole. TOCTOU note: the transport re-resolves at connect; the
/// window is accepted for these lanes until their clients take pinned
/// addresses.
pub fn vet_host(host: &str, port: u16, gate: &dyn AddressGate) -> Result<(), crate::IoError> {
    resolve_vetted(host, port, gate).map(|_| ())
}

/// [`vet_host`], returning the first vetted address so a transport that owns
/// its own sockets (librist) can be pinned to the checked literal instead of
/// re-resolving the hostname — the same TOCTOU close as the pinned-IP HTTP
/// connect.
pub fn resolve_vetted(
    host: &str,
    port: u16,
    gate: &dyn AddressGate,
) -> Result<std::net::SocketAddr, crate::IoError> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !gate.permit(ip) {
            return Err(crate::IoError::new(
                crate::IoErrorKind::Blocked,
                format!("{ip} blocked"),
            ));
        }
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| crate::IoError::new(crate::IoErrorKind::Resolve, format!("{host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(crate::IoError::new(
            crate::IoErrorKind::Resolve,
            format!("{host}: no addresses"),
        ));
    }
    for addr in &addrs {
        if !gate.permit(addr.ip()) {
            return Err(crate::IoError::new(
                crate::IoErrorKind::Blocked,
                format!("{host} resolves to blocked {}", addr.ip()),
            ));
        }
    }
    Ok(addrs[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(s: &str) -> bool {
        !PublicAddressGate.permit(s.parse().unwrap())
    }

    #[test]
    fn private_and_special_ranges_are_blocked() {
        for addr in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "192.0.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2001:db8::1",
        ] {
            assert!(blocked(addr), "{addr} must be blocked");
        }
    }

    #[test]
    fn public_addresses_pass() {
        for addr in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.128.0.1",
            "172.32.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(!blocked(addr), "{addr} must pass");
        }
    }
}
