//! Which URLs the engine is allowed to talk to.
//!
//! A download manager is a ready-made SSRF amplifier: it fetches whatever URL it is
//! given and writes the answer to disk. The review (R5) asks for the decision to be
//! made on the **resolved address**, at connect time, on every hop — not on the host
//! name, which a rebinding record can change between the check and the request.
//!
//! [`UrlPolicy`] is the pure part: it decides about a scheme and an address. The
//! engine wires it into the HTTP client.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// Refused because the address is this machine or the local network.
    PrivateAddress(IpAddr),
    /// Refused because the scheme is not http or https.
    Scheme,
    /// The URL has no host at all.
    NoHost,
}

#[derive(Clone, Debug, Default)]
pub struct UrlPolicy {
    /// When true, a caller who has asked the user explicitly may reach loopback and
    /// private ranges. Default false; the UI turns it on for one task at a time.
    pub allow_private: bool,
}

impl UrlPolicy {
    pub fn permissive() -> Self {
        Self {
            allow_private: true,
        }
    }

    /// Checks the scheme and, when the URL holds a literal address, the address.
    ///
    /// A host name is **not** resolved here: resolving belongs to the connection, and
    /// [`classify`] is what the engine calls with the address it actually dialled.
    pub fn check_url(&self, url: &Url) -> Verdict {
        if !matches!(url.scheme(), "http" | "https") {
            return Verdict::Scheme;
        }
        match url.host() {
            None => Verdict::NoHost,
            Some(Host::Ipv4(ip)) => self.check_addr(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => self.check_addr(IpAddr::V6(ip)),
            // `localhost` is a name, but a well-known one; refuse it without an
            // explicit permission so a rebinding setup does not even get a chance.
            Some(Host::Domain(name)) => {
                let lower = name.trim_end_matches('.').to_ascii_lowercase();
                if (lower == "localhost" || lower.ends_with(".localhost")) && !self.allow_private {
                    Verdict::PrivateAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))
                } else {
                    Verdict::Allowed
                }
            }
        }
    }

    pub fn check_addr(&self, ip: IpAddr) -> Verdict {
        if self.allow_private || is_public(ip) {
            Verdict::Allowed
        } else {
            Verdict::PrivateAddress(ip)
        }
    }
}

/// True for addresses that are, or may forward to, something on the local network.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254.0.0/16, the cloud metadata address
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        // 0.0.0.0/8 "this network"
        || a == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (a == 100 && (64..=127).contains(&b))
        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15 benchmarking
        || (a == 198 && (b == 18 || b == 19))
        // 240.0.0.0/4 reserved
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // An IPv4-mapped or IPv4-compatible address is judged as the v4 address it is.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // fc00::/7 unique local
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 link local
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32 documentation
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // ::ffff:0:0/96 and other transition prefixes are handled above or refused
        || (segments[0] == 0 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0
            && segments[4] == 0 && segments[5] == 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn scheme_must_be_http_or_https() {
        let policy = UrlPolicy::default();
        assert_eq!(
            policy.check_url(&url("ftp://example.com/a")),
            Verdict::Scheme
        );
        assert_eq!(
            policy.check_url(&url("file:///etc/passwd")),
            Verdict::Scheme
        );
        assert_eq!(
            policy.check_url(&url("data:text/plain,hi")),
            Verdict::Scheme
        );
        assert_eq!(
            policy.check_url(&url("https://example.com/a")),
            Verdict::Allowed
        );
    }

    #[test]
    fn literal_loopback_and_metadata_addresses_are_refused() {
        let policy = UrlPolicy::default();
        for hostile in [
            "http://127.0.0.1:8080/x",
            "http://127.1.2.3/x",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://172.16.0.1/x",
            "http://100.64.0.1/x",
            "http://0.0.0.0/x",
            "http://[::1]/x",
            "http://[fe80::1]/x",
            "http://[fd00::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://localhost/x",
            "http://LOCALHOST./x",
            "http://a.localhost/x",
        ] {
            assert!(
                matches!(policy.check_url(&url(hostile)), Verdict::PrivateAddress(_)),
                "{hostile} should be refused"
            );
        }
    }

    #[test]
    fn public_literals_and_names_pass() {
        let policy = UrlPolicy::default();
        for ok in [
            "http://1.1.1.1/x",
            "http://[2606:4700::1111]/x",
            "https://example.com/x",
        ] {
            assert_eq!(policy.check_url(&url(ok)), Verdict::Allowed, "{ok}");
        }
    }

    #[test]
    fn explicit_permission_opens_private_ranges() {
        let policy = UrlPolicy::permissive();
        assert_eq!(
            policy.check_url(&url("http://127.0.0.1:9000/x")),
            Verdict::Allowed
        );
        assert_eq!(
            policy.check_url(&url("http://localhost:9000/x")),
            Verdict::Allowed
        );
        // The scheme stays closed even with permission.
        assert_eq!(
            policy.check_url(&url("file:///etc/passwd")),
            Verdict::Scheme
        );
    }

    #[test]
    fn every_octet_of_private_v4_is_classified() {
        for (text, expected) in [
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("93.184.216.34", true),
            ("127.0.0.1", false),
            ("10.255.255.255", false),
            ("192.168.0.1", false),
            ("172.31.255.255", false),
            ("172.32.0.1", true),
            ("169.254.169.254", false),
            ("255.255.255.255", false),
        ] {
            let ip: Ipv4Addr = text.parse().unwrap();
            assert_eq!(is_public_v4(ip), expected, "{text}");
        }
    }
}
