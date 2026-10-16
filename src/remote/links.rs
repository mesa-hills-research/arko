//! The links to the page, ranked for the device that opens one (a phone,
//! say) and for the QR code, which shows the first: real network
//! interfaces on a private network first, Tailscale next, VPN tunnels last.
//! A VPN's address is one the other devices on the network can't reach,
//! and it can be a private address like the network's own (Mullvad gives
//! 10.64.x.x), so interface names count as well as addresses.
//!
//! `ARKO_REMOTE_HOST` (an address or an interface name, in the environment
//! or a `.env`) puts its link first, whatever the ranking says.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::Serialize;

/// The variable that picks the link shown first, and as the QR code.
pub const HOST_VARIABLE: &str = "ARKO_REMOTE_HOST";

/// What kind of network a link is on, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A private address on a physical interface: the local network.
    Lan,
    /// Tailscale's network (100.64.0.0/10, `tailscale0`).
    Tailscale,
    /// Anything else: a public address, a virtual bridge (Docker, VMs).
    Other,
    /// A VPN tunnel (`wg*`, `tun*`, `utun*`, Mullvad): rarely reachable
    /// from the other devices.
    Vpn,
    /// This machine only.
    Loopback,
}

impl Kind {
    /// How the terminal names it.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Lan => "LAN",
            Kind::Tailscale => "Tailscale",
            Kind::Other => "other",
            Kind::Vpn => "VPN",
            Kind::Loopback => "this machine",
        }
    }
}

/// A network interface's address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub ip: IpAddr,
    /// Whether it is a physical device; `None` where that isn't known
    /// (Linux tells, in `/sys/class/net/<name>/device`).
    pub physical: Option<bool>,
}

/// The interfaces of this machine with an address.
pub fn interfaces() -> Vec<Interface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|interface| Interface {
            physical: physical(&interface.name),
            ip: interface.ip(),
            name: interface.name,
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn physical(name: &str) -> Option<bool> {
    Some(
        std::path::Path::new("/sys/class/net")
            .join(name)
            .join("device")
            .exists(),
    )
}

#[cfg(not(target_os = "linux"))]
fn physical(_: &str) -> Option<bool> {
    None
}

/// The kind of network `interface` is on.
pub fn kind(interface: &Interface) -> Kind {
    let name = interface.name.to_ascii_lowercase();
    if interface.ip.is_loopback() {
        Kind::Loopback
    } else if name.contains("tailscale") || is_tailscale(interface.ip) {
        Kind::Tailscale
    } else if is_tunnel(&name) {
        Kind::Vpn
    } else if is_private(interface.ip) && interface.physical != Some(false) {
        Kind::Lan
    } else {
        Kind::Other
    }
}

/// Tailscale's addresses: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64,
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// A VPN tunnel's name: WireGuard (`wg0`, `wg0-mullvad`), TUN and TAP
/// devices (`tun0`, macOS's `utun3`), PPP, or a VPN's own name, as on
/// Windows.
fn is_tunnel(name: &str) -> bool {
    const PREFIXES: [&str; 6] = ["wg", "tun", "utun", "tap", "ppp", "ipsec"];
    const NAMES: [&str; 5] = ["mullvad", "wireguard", "vpn", "nordlynx", "proton"];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || NAMES.iter().any(|word| name.contains(word))
}

/// A private address: 10/8, 172.16/12, 192.168/16, or an IPv6 unique local
/// one (fc00::/7).
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// A link to the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub url: String,
    /// The network interface, when the server listens on all of them.
    pub interface: Option<String>,
    pub kind: Kind,
    pub loopback: bool,
}

impl Link {
    fn new(addr: SocketAddr, key: &str, ip: IpAddr, interface: Option<String>, kind: Kind) -> Self {
        let host = match ip {
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        };
        Link {
            url: format!("http://{host}:{}/#key={key}", addr.port()),
            interface,
            kind,
            loopback: ip.is_loopback(),
        }
    }

    /// Whether `host` (an interface's name or an address) names this link.
    pub fn is(&self, host: &str) -> bool {
        let host = host.trim();
        self.interface
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(host))
            || host.parse::<IpAddr>().is_ok_and(|ip| {
                let shown = match ip {
                    IpAddr::V6(ip) => format!("[{ip}]"),
                    IpAddr::V4(ip) => ip.to_string(),
                };
                self.url.starts_with(&format!("http://{shown}:"))
            })
    }
}

/// The links to a server on `addr`, with `key` in their fragments, best
/// first: on every interface's address (of `interfaces`) when it listens on
/// all of them, ranked by [`Kind`], this machine's last.
pub fn links(addr: SocketAddr, key: &str, interfaces: Vec<Interface>) -> Vec<Link> {
    if !addr.ip().is_unspecified() {
        let kind = if addr.ip().is_loopback() {
            Kind::Loopback
        } else {
            // The one address it was asked to listen on.
            Kind::Lan
        };
        return vec![Link::new(addr, key, addr.ip(), None, kind)];
    }
    let v6 = addr.ip().is_ipv6();
    let mut links: Vec<Link> = interfaces
        .into_iter()
        .filter(|interface| !interface.ip.is_loopback())
        .filter(|interface| match interface.ip {
            IpAddr::V4(_) => !v6,
            // Link-local addresses need a zone in the URL.
            IpAddr::V6(ip) => v6 && (ip.segments()[0] & 0xffc0) != 0xfe80,
        })
        .map(|interface| {
            let kind = kind(&interface);
            Link::new(addr, key, interface.ip, Some(interface.name), kind)
        })
        .collect();
    links.dedup_by(|a, b| a.url == b.url);
    // Stable: interfaces of a kind keep the system's order.
    links.sort_by_key(|link| link.kind);
    let local = if v6 {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    links.push(Link::new(addr, key, local, None, Kind::Loopback));
    links
}

/// Puts the link `host` names first. `Err` with what to tell the user when
/// none has it.
pub fn prefer(links: &mut [Link], host: &str) -> Result<(), String> {
    match links.iter().position(|link| link.is(host)) {
        Some(index) => {
            links[..=index].rotate_right(1);
            Ok(())
        }
        None => Err(format!(
            "{HOST_VARIABLE} is `{host}`, which none of the page's links has ({}): the links are in their usual order.",
            names(links)
        )),
    }
}

/// The interfaces (else the addresses) of the links other devices can
/// open, as the user is told them.
pub fn names(links: &[Link]) -> String {
    let names: Vec<String> = links
        .iter()
        .filter(|link| !link.loopback)
        .map(|link| match &link.interface {
            Some(name) => name.clone(),
            // The address, without the key.
            None => link.url.split("/#").next().unwrap_or_default().to_string(),
        })
        .collect();
    if names.is_empty() {
        "none but this machine's".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(name: &str, ip: &str, physical: Option<bool>) -> Interface {
        Interface {
            name: name.into(),
            ip: ip.parse().unwrap(),
            physical,
        }
    }

    fn order(interfaces: Vec<Interface>) -> Vec<(String, Kind)> {
        links("0.0.0.0:8765".parse().unwrap(), "k", interfaces)
            .into_iter()
            .map(|link| (link.interface.unwrap_or_default(), link.kind))
            .collect()
    }

    /// Mullvad's WireGuard tunnel has a private address like the network's,
    /// and comes first in the system's list: the network's interface wins,
    /// whether Linux says which device is physical or the system doesn't.
    #[test]
    fn the_network_wins_over_a_mullvad_tunnel() {
        for physical in [Some(true), None] {
            let tunnel = physical.map(|_| false);
            let ranked = order(vec![
                interface("wg0-mullvad", "10.64.12.34", tunnel),
                interface("ens33", "192.168.1.12", physical),
                interface("lo", "127.0.0.1", tunnel),
            ]);
            assert_eq!(
                ranked,
                [
                    ("ens33".to_string(), Kind::Lan),
                    ("wg0-mullvad".to_string(), Kind::Vpn),
                    (String::new(), Kind::Loopback),
                ],
                "{physical:?}"
            );
        }
    }

    /// The network first, Tailscale next (by name or by address, as macOS's
    /// `utun` has it), then virtual bridges and public addresses, VPN
    /// tunnels last, this machine after them.
    #[test]
    fn links_are_ranked_by_the_network_they_are_on() {
        let ranked = order(vec![
            interface("docker0", "172.17.0.1", Some(false)),
            interface("tun0", "10.8.0.2", Some(false)),
            interface("tailscale0", "100.101.102.103", Some(false)),
            interface("wlan0", "192.168.1.30", Some(true)),
            interface("utun4", "100.70.1.2", None),
            interface("Mullvad", "10.66.0.9", None),
            interface("enp3s0", "203.0.113.7", Some(true)),
            interface("eth0", "10.0.0.5", Some(true)),
            interface("ens33", "169.254.10.1", Some(true)),
        ]);
        let ranked: Vec<(&str, Kind)> = ranked
            .iter()
            .map(|(name, kind)| (name.as_str(), *kind))
            .collect();
        assert_eq!(
            ranked,
            [
                ("wlan0", Kind::Lan),
                ("eth0", Kind::Lan),
                ("tailscale0", Kind::Tailscale),
                ("utun4", Kind::Tailscale),
                ("docker0", Kind::Other),
                ("enp3s0", Kind::Other),
                ("ens33", Kind::Other),
                ("tun0", Kind::Vpn),
                ("Mullvad", Kind::Vpn),
                ("", Kind::Loopback),
            ]
        );
        assert_eq!(
            kind(&interface("tailscale0", "fd7a:115c:a1e0::1", None)),
            Kind::Tailscale
        );
        assert_eq!(
            kind(&interface("eth0", "fd12:3456::1", Some(true))),
            Kind::Lan
        );
    }

    /// `ARKO_REMOTE_HOST` puts the link it names first, by interface or
    /// address; one that names none says so and changes nothing.
    #[test]
    fn the_host_named_comes_first() {
        let mut ranked = links(
            "0.0.0.0:8765".parse().unwrap(),
            "k",
            vec![
                interface("ens33", "192.168.1.12", Some(true)),
                interface("tailscale0", "100.101.102.103", Some(false)),
                interface("wg0-mullvad", "10.64.12.34", Some(false)),
            ],
        );
        prefer(&mut ranked, "TAILSCALE0").unwrap();
        assert_eq!(ranked[0].interface.as_deref(), Some("tailscale0"));
        assert_eq!(ranked[1].interface.as_deref(), Some("ens33"));
        prefer(&mut ranked, "10.64.12.34").unwrap();
        assert_eq!(ranked[0].interface.as_deref(), Some("wg0-mullvad"));
        let before = ranked.clone();
        let error = prefer(&mut ranked, "wlan9").unwrap_err();
        assert_eq!(
            error,
            "ARKO_REMOTE_HOST is `wlan9`, which none of the page's links has (wg0-mullvad, tailscale0, ens33): the links are in their usual order."
        );
        assert_eq!(ranked, before);
    }

    #[test]
    fn a_server_on_one_address_has_one_link() {
        let local = links("127.0.0.1:8765".parse().unwrap(), "abc", Vec::new());
        assert_eq!(
            local,
            [Link {
                url: "http://127.0.0.1:8765/#key=abc".into(),
                interface: None,
                kind: Kind::Loopback,
                loopback: true,
            }]
        );
        assert_eq!(names(&local), "none but this machine's");
        let one = links("192.168.1.12:9000".parse().unwrap(), "abc", Vec::new());
        assert_eq!(one[0].kind, Kind::Lan);
        assert!(one[0].is("192.168.1.12"));
    }
}
