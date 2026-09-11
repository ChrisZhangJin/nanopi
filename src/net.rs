//! Shared HTTP client construction, and the DNS policy behind it.
//!
//! # Why this module exists
//!
//! `reqwest` is built with the `hickory-dns` feature, which makes the
//! pure-Rust resolver the default (`reqwest`'s `client.rs`:
//! `hickory_dns: cfg!(feature = "hickory-dns")`). That was deliberate —
//! 96aa4e4 added it so the wizard's probe survives a poisoned system
//! resolver — but hickory gets its nameservers from exactly one place:
//!
//! ```text
//! hickory-resolver/src/system_conf/unix.rs:32
//!     read_resolv_conf("/etc/resolv.conf")
//! ```
//!
//! Android has no `/etc/resolv.conf`. `/etc` is a symlink to a read-only
//! `/system/etc`, and DNS is done by netd over binder through bionic's
//! `android_getaddrinfo`. There is no file to read, so every request
//! failed with:
//!
//! ```text
//! error reading DNS system conf for hickory-dns: io error: os error 2
//! ```
//!
//! Note that dropping the hickory feature does NOT fix this for the
//! binaries we actually ship. A static musl build falls back to musl's
//! own resolver, which reads the same missing `/etc/resolv.conf` and
//! then defaults to `127.0.0.1:53` — nothing is listening there on a
//! phone. Only a bionic-linked (NDK) build reaches netd. So the musl
//! artifacts need nameservers supplied out-of-band, which is this
//! module.
//!
//! # Resolution order
//!
//! 1. `$NANOPI_DNS` — comma or space separated, `IP` or `IP:port`.
//!    The literal value `system` forces reqwest's default behaviour.
//! 2. `<nanopi home>/resolv.conf` — standard `nameserver <ip>` syntax.
//!    Deliberately the same path the Termux launcher already writes, so
//!    a working proot setup keeps working once proot is dropped.
//! 3. `/etc/resolv.conf` present → leave reqwest alone; hickory reads it
//!    itself, and the system's own resolver list is the right default on
//!    every normal host.
//! 4. Nothing → leave reqwest alone (it will produce the error above)
//!    and emit a note naming the fix, once. We do NOT invent a default
//!    nameserver: picking 8.8.8.8 is wrong behind the GFW, picking a
//!    Chinese resolver is wrong everywhere else, and silently routing a
//!    user's DNS through an unrequested third party is worse than an
//!    error that says what to do.
//!
//! This is a process-global policy read from the environment rather than
//! a config key, because the wizard probes a provider BEFORE any config
//! file exists (`wizard.rs`), and that probe is the first thing a new
//! Android user hits. A `config.toml` setting could not be read in time
//! to help.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use hickory_resolver::config::{LookupIpStrategy, NameServerConfig, ResolverConfig};
use hickory_resolver::lookup_ip::LookupIpIntoIter;
use hickory_resolver::name_server::TokioConnectionProvider;
use hickory_resolver::proto::xfer::Protocol;
use hickory_resolver::TokioResolver;

/// Port assumed for a nameserver given without one.
const DNS_PORT: u16 = 53;

/// Per-query timeout and attempt count for the explicit resolver.
///
/// hickory's defaults (5 s × 2 attempts) let one unreachable nameserver
/// stall a turn for ~15 s before the first error reaches the user —
/// measured, not estimated. That is tolerable for a daemon and much too
/// long for a CLI where the user is watching a cursor. 3 s × 2 brings a
/// dead nameserver down to ~9 s measured (not the ~6 s the arithmetic
/// suggests — hickory issues A and AAAA separately), while still
/// surviving a dropped UDP packet on a mobile network, which is exactly
/// the setting this resolver exists for. Dropping to 1 attempt measures
/// ~6 s and was rejected: one lost packet should not fail a turn.
const DNS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const DNS_ATTEMPTS: usize = 2;

/// A `reqwest::ClientBuilder` with this binary's DNS policy applied.
///
/// Every HTTP client in the process must come from here. Three call
/// sites used to call `reqwest::Client::builder()` directly, which meant
/// a DNS fix applied to the provider clients would silently not apply to
/// the WASM plugin fetch client.
pub fn client_builder() -> reqwest::ClientBuilder {
    let b = reqwest::Client::builder();
    match dns_plan() {
        DnsPlan::System => b,
        DnsPlan::Explicit(servers) => b.dns_resolver(Arc::new(ExplicitResolver {
            servers: servers.clone(),
            state: Arc::new(OnceLock::new()),
        })),
    }
}

/// What the resolver should do. `System` covers both "a normal host with
/// /etc/resolv.conf" and "nothing configured anywhere" — in the second
/// case the note has already been emitted and reqwest's own error is the
/// most accurate thing we can surface.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsPlan {
    System,
    Explicit(Vec<SocketAddr>),
}

fn dns_plan() -> DnsPlan {
    static PLAN: OnceLock<DnsPlan> = OnceLock::new();
    PLAN.get_or_init(|| {
        let plan = decide(
            std::env::var("NANOPI_DNS").ok(),
            crate::paths::nanopi_home().map(|h| h.join("resolv.conf")),
            std::path::Path::new("/etc/resolv.conf").is_file(),
        );
        if let DnsPlan::System = plan {
            // Only warn in the genuinely broken case: no explicit
            // servers AND no system file. A normal host takes the same
            // branch and must stay silent.
            if !std::path::Path::new("/etc/resolv.conf").is_file()
                && std::env::var_os("NANOPI_DNS").is_none()
            {
                crate::note!(
                    "no /etc/resolv.conf and no NANOPI_DNS — DNS will fail on this host \
                     (common on Android). Set NANOPI_DNS=223.5.5.5,119.29.29.29 or write \
                     nameserver lines to ~/.nanopi/resolv.conf"
                );
            }
        }
        plan
    })
    .clone()
}

/// The decision itself, with every input injected.
///
/// Split from `dns_plan` because that memoizes process-wide: a test that
/// set `$NANOPI_DNS` would either read a value cached by an earlier test
/// or poison a later one, and the interesting cases are exactly the ones
/// that need different environments.
fn decide(env: Option<String>, home_resolv: Option<PathBuf>, etc_resolv: bool) -> DnsPlan {
    if let Some(raw) = env {
        let raw = raw.trim();
        // An explicit opt-out, for someone who wants the system resolver
        // on a host where we would otherwise pick up a stale
        // ~/.nanopi/resolv.conf.
        if raw.eq_ignore_ascii_case("system") {
            return DnsPlan::System;
        }
        let servers = parse_server_list(raw);
        if !servers.is_empty() {
            return DnsPlan::Explicit(servers);
        }
        // A set-but-unparseable value falls through rather than
        // silently meaning "system": the note below, or reqwest's own
        // error, is more useful than pretending the variable was unset.
    }
    if let Some(path) = home_resolv {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let servers = parse_resolv_conf(&text);
            if !servers.is_empty() {
                return DnsPlan::Explicit(servers);
            }
        }
    }
    let _ = etc_resolv;
    DnsPlan::System
}

/// `1.1.1.1, 8.8.8.8:5353` → two addrs. Bare IPv6 must be bracketed to
/// carry a port, exactly as in a URL; an unbracketed IPv6 is accepted as
/// a plain address on port 53.
fn parse_server_list(raw: &str) -> Vec<SocketAddr> {
    raw.split([',', ' ', '\t', ';'])
        .filter(|s| !s.is_empty())
        .filter_map(parse_one)
        .collect()
}

fn parse_one(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Some(SocketAddr::new(ip, DNS_PORT));
    }
    None
}

/// Minimal `resolv.conf` reader: `nameserver <ip>` lines only.
///
/// Deliberately not the `resolv-conf` crate even though it is already in
/// the tree via hickory. We consume exactly one directive, and the
/// options we would otherwise inherit (`ndots`, `search`, `rotate`)
/// change lookup semantics in ways that are not worth taking on for a
/// file this binary wrote itself.
fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| l.strip_prefix("nameserver"))
        .filter_map(|rest| parse_one(rest.trim()))
        .collect()
}

/// `reqwest::dns::Resolve` over a fixed nameserver list.
///
/// Mirrors reqwest's own `HickoryDnsResolver`, including the delayed
/// construction: `client_builder()` can be called outside a Tokio
/// runtime, and building the resolver eagerly would panic there.
#[derive(Debug, Clone)]
struct ExplicitResolver {
    servers: Vec<SocketAddr>,
    state: Arc<OnceLock<TokioResolver>>,
}

impl reqwest::dns::Resolve for ExplicitResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let this = self.clone();
        Box::pin(async move {
            let resolver = this.state.get_or_init(|| {
                let mut cfg = ResolverConfig::new();
                for addr in &this.servers {
                    // UDP with a TCP retry is what a stock resolver does;
                    // hickory falls back to TCP itself on a truncated
                    // response, so only UDP needs declaring.
                    cfg.add_name_server(NameServerConfig::new(*addr, Protocol::Udp));
                }
                let mut builder =
                    TokioResolver::builder_with_config(cfg, TokioConnectionProvider::default());
                // Match reqwest's own setting so "happy eyeballs" still
                // works and an IPv6-only provider stays reachable.
                builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4AndIpv6;
                builder.options_mut().timeout = DNS_TIMEOUT;
                builder.options_mut().attempts = DNS_ATTEMPTS;
                builder.build()
            });
            let lookup = resolver.lookup_ip(name.as_str()).await?;
            let addrs: reqwest::dns::Addrs = Box::new(SocketAddrs {
                iter: lookup.into_iter(),
            });
            Ok(addrs)
        })
    }
}

struct SocketAddrs {
    iter: LookupIpIntoIter,
}

impl Iterator for SocketAddrs {
    type Item = SocketAddr;
    fn next(&mut self) -> Option<Self::Item> {
        // Port 0: reqwest substitutes the scheme's conventional port.
        self.iter.next().map(|ip| SocketAddr::new(ip, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn env_list_accepts_separators_and_optional_ports() {
        assert_eq!(
            parse_server_list("1.1.1.1, 8.8.8.8:5353 9.9.9.9"),
            vec![sa("1.1.1.1:53"), sa("8.8.8.8:5353"), sa("9.9.9.9:53")]
        );
    }

    #[test]
    fn ipv6_needs_brackets_only_for_a_port() {
        assert_eq!(parse_server_list("2400:3200::1"), vec![sa("[2400:3200::1]:53")]);
        assert_eq!(
            parse_server_list("[2400:3200::1]:5353"),
            vec![sa("[2400:3200::1]:5353")]
        );
    }

    #[test]
    fn garbage_entries_are_dropped_not_guessed() {
        assert!(parse_server_list("not-an-ip, dns.example.com").is_empty());
        // A hostname cannot be a bootstrap nameserver — resolving it
        // would need the resolver we are trying to build.
        assert_eq!(parse_server_list("dns.example.com, 1.1.1.1"), vec![sa("1.1.1.1:53")]);
    }

    #[test]
    fn resolv_conf_reads_nameserver_lines_only() {
        let text = "\
# a comment
search lan
nameserver 223.5.5.5
options ndots:2
nameserver 119.29.29.29  # trailing comment
domain example.com
";
        assert_eq!(
            parse_resolv_conf(text),
            vec![sa("223.5.5.5:53"), sa("119.29.29.29:53")]
        );
    }

    /// The Android case this module was written for: no /etc/resolv.conf
    /// and no home file, so we stay on `System` and let the note plus
    /// reqwest's own error explain it. Asserting that we do NOT silently
    /// invent a nameserver.
    #[test]
    fn android_with_nothing_configured_stays_on_system() {
        assert_eq!(decide(None, None, false), DnsPlan::System);
    }

    /// And the actual fix path: an env var alone is enough, with no
    /// files involved anywhere.
    #[test]
    fn env_alone_fixes_a_host_with_no_resolv_conf() {
        assert_eq!(
            decide(Some("223.5.5.5,119.29.29.29".into()), None, false),
            DnsPlan::Explicit(vec![sa("223.5.5.5:53"), sa("119.29.29.29:53")])
        );
    }

    #[test]
    fn env_beats_the_home_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("resolv.conf");
        std::fs::write(&f, "nameserver 10.0.0.1\n").unwrap();
        assert_eq!(
            decide(Some("1.1.1.1".into()), Some(f), false),
            DnsPlan::Explicit(vec![sa("1.1.1.1:53")])
        );
    }

    #[test]
    fn home_file_is_used_when_env_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("resolv.conf");
        std::fs::write(&f, "nameserver 10.0.0.1\nnameserver 10.0.0.2\n").unwrap();
        assert_eq!(
            decide(None, Some(f), false),
            DnsPlan::Explicit(vec![sa("10.0.0.1:53"), sa("10.0.0.2:53")])
        );
    }

    /// `system` is the opt-out for a host that has a working
    /// /etc/resolv.conf but a stale ~/.nanopi/resolv.conf left behind by
    /// the Termux launcher.
    #[test]
    fn explicit_system_ignores_a_present_home_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("resolv.conf");
        std::fs::write(&f, "nameserver 10.0.0.1\n").unwrap();
        assert_eq!(decide(Some("system".into()), Some(f), true), DnsPlan::System);
        assert_eq!(decide(Some("SYSTEM".into()), None, true), DnsPlan::System);
    }

    /// An empty or comment-only file must not count as "configured" —
    /// otherwise a touched file would produce a resolver with zero
    /// nameservers, which hangs rather than fails.
    #[test]
    fn empty_home_file_falls_through_to_system() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("resolv.conf");
        std::fs::write(&f, "# nothing here\n\n").unwrap();
        assert_eq!(decide(None, Some(f), true), DnsPlan::System);
    }

    /// A one-shot DNS server that answers A queries with `answer` and
    /// AAAA queries with an empty NOERROR. Returns its bound address.
    ///
    /// Hermetic on purpose. The alternative — pointing the test at the
    /// host's real nameserver — would make it depend on outbound UDP 53
    /// and on a name that resolves everywhere, neither of which holds in
    /// every environment this repo is built in. A local socket also
    /// proves the thing actually worth proving: that the query goes to
    /// the address we configured, rather than to whatever the system
    /// resolver would have used.
    async fn fake_dns(answer: std::net::Ipv4Addr) -> SocketAddr {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, peer)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                let q = &buf[..n];
                if n < 12 {
                    continue;
                }
                // Walk the QNAME labels to find where the question ends.
                let mut i = 12;
                while i < n && q[i] != 0 {
                    i += 1 + q[i] as usize;
                }
                let qend = i + 1 + 4; // null label + QTYPE + QCLASS
                if qend > n {
                    continue;
                }
                let qtype = u16::from_be_bytes([q[qend - 4], q[qend - 3]]);
                let mut r = Vec::with_capacity(64);
                r.extend_from_slice(&q[0..2]); // echo the transaction id
                r.extend_from_slice(&0x8180u16.to_be_bytes()); // QR|RD|RA, NOERROR
                r.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
                let is_a = qtype == 1;
                r.extend_from_slice(&(is_a as u16).to_be_bytes()); // ANCOUNT
                r.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
                r.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
                r.extend_from_slice(&q[12..qend]); // question, verbatim
                if is_a {
                    r.extend_from_slice(&0xC00Cu16.to_be_bytes()); // ptr to qname
                    r.extend_from_slice(&1u16.to_be_bytes()); // type A
                    r.extend_from_slice(&1u16.to_be_bytes()); // class IN
                    r.extend_from_slice(&60u32.to_be_bytes()); // ttl
                    r.extend_from_slice(&4u16.to_be_bytes()); // rdlength
                    r.extend_from_slice(&answer.octets());
                }
                let _ = sock.send_to(&r, peer).await;
            }
        });
        addr
    }

    /// End to end: an `ExplicitResolver` really does query the
    /// nameserver it was handed and really does parse the reply. Every
    /// other test in this module checks a pure function; without this
    /// one, the whole module could be plumbed into reqwest incorrectly
    /// and still look green.
    #[tokio::test]
    async fn explicit_resolver_queries_the_configured_server() {
        use reqwest::dns::Resolve;
        let want = std::net::Ipv4Addr::new(203, 0, 113, 7);
        let ns = fake_dns(want).await;
        let resolver = ExplicitResolver {
            servers: vec![ns],
            state: Arc::new(OnceLock::new()),
        };
        let name: reqwest::dns::Name = "probe.nanopi.example.com".parse().unwrap();
        let addrs: Vec<SocketAddr> = resolver.resolve(name).await.unwrap().collect();
        assert!(
            addrs.iter().any(|a| a.ip() == std::net::IpAddr::V4(want)),
            "resolver did not return the answer our nameserver gave: {addrs:?}"
        );
    }

    /// And it must NOT fall back to the system resolver when the
    /// configured server is dead — a silent fallback would reintroduce
    /// exactly the Android failure this module exists to fix, except
    /// harder to see.
    #[tokio::test]
    async fn a_dead_nameserver_fails_rather_than_falling_back() {
        use reqwest::dns::Resolve;
        // Port 1 on loopback: nothing listens, ICMP port-unreachable.
        let resolver = ExplicitResolver {
            servers: vec![sa("127.0.0.1:1")],
            state: Arc::new(OnceLock::new()),
        };
        let name: reqwest::dns::Name = "probe.nanopi.example.com".parse().unwrap();
        assert!(resolver.resolve(name).await.is_err());
    }

    /// A set-but-unparseable NANOPI_DNS must not be read as "system":
    /// the user clearly meant to configure something.
    #[test]
    fn unparseable_env_does_not_masquerade_as_system() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("resolv.conf");
        std::fs::write(&f, "nameserver 10.0.0.1\n").unwrap();
        assert_eq!(
            decide(Some("nonsense".into()), Some(f), false),
            DnsPlan::Explicit(vec![sa("10.0.0.1:53")])
        );
    }
}
