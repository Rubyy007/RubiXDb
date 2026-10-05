//! Loopback-only port binding with real collision handling -- bind
//! once, keep the exact listener, hand it to the server. No
//! bind-probe-then-rebind TOCTOU window (`PHASE_RUBIXDB_INSTANCE_
//! ARCHITECTURE.md` §5): the caller gets back the live
//! `std::net::TcpListener` it will actually serve on.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

/// Canonical default: `127.0.0.1:302`. This is a platform-specific
/// choice, not a portable one -- ports 0-1023 are OS-privileged on
/// Linux/macOS (binding one requires root/`CAP_NET_BIND_SERVICE`), so
/// a normal user's `rubixdb gui`/`rubixdb cli` would fail to bind it
/// there and always fall through to the ephemeral-port path below
/// instead. Accepted as a documented, non-blocking limitation for a
/// Windows-only product target (Windows has no such restriction);
/// `bind_loopback` logs a clear diagnostic rather than silently
/// falling back when this is the specific cause, so the gap is
/// visible rather than hidden. See `PHASE_RUBIXDB_INSTANCE_
/// ARCHITECTURE.md` §5.
pub const DEFAULT_API_PORT: u16 = 302;

/// Tries `preferred_port` on `127.0.0.1` first (keeps behavior
/// predictable across restarts of the same instance -- a previously
/// persisted `instance.json` gets its old port back whenever nothing
/// else has taken it). On any bind failure (most commonly
/// `AddrInUse`, e.g. two `default` instances racing, or an unrelated
/// process already on the preferred port; on a non-Windows platform,
/// binding a privileged port as a non-root user also lands here, as
/// `PermissionDenied`) falls back to `127.0.0.1:0`, letting the OS
/// assign a free ephemeral port -- the actually-bound port is what
/// gets persisted to `instance.json`, never the preference.
///
/// item 43 (loopback-only, hard security gate): this function has no
/// parameter for the bind address -- `127.0.0.1` is not
/// configurable here, by construction, not by a default that could be
/// overridden.
pub fn bind_loopback(preferred_port: u16) -> std::io::Result<TcpListener> {
    let preferred = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, preferred_port));
    match TcpListener::bind(preferred) {
        Ok(listener) => Ok(listener),
        Err(e) => {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!(
                    "rubixdb: could not bind the canonical port 127.0.0.1:{preferred_port} \
                     ({e}) -- on Linux/macOS, ports below 1024 require root/CAP_NET_BIND_SERVICE; \
                     falling back to an OS-assigned port instead"
                );
            }
            let ephemeral = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
            TcpListener::bind(ephemeral)
        }
    }
}

/// Binds exactly `port` on loopback, or returns `None` (no fallback).
pub fn try_bind_exact(port: u16) -> Option<TcpListener> {
    TcpListener::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))).ok()
}

/// Port for an instance that already has a persisted manifest port. The
/// `default` instance goes back to the canonical port whenever it is free: an
/// earlier run that fell back to a random port (because something else held
/// 302 at the time) must not pin the instance to that random port forever.
/// Every other instance keeps its own persisted port, as before. `canonical`
/// is a parameter only so tests need not touch the real port 302.
pub fn bind_for_existing(
    name: &str,
    persisted: u16,
    canonical: u16,
) -> std::io::Result<TcpListener> {
    // ASCII case-insensitive: NTFS resolves `DEFAULT` and `default` to the same
    // instance directory, so the rule must not depend on how the name was typed.
    if name.eq_ignore_ascii_case(crate::paths::DEFAULT_INSTANCE_NAME) && persisted != canonical {
        if let Some(listener) = try_bind_exact(canonical) {
            return Ok(listener);
        }
    }
    bind_loopback(persisted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase AF evidence: the canonical default is exactly 302
    /// decimal. Rust integer literals are always base-10 unless
    /// explicitly prefixed (`0o302` for octal, `0x302` for hex) --
    /// `302` in source is never at risk of octal reinterpretation, and
    /// this test pins the literal value itself so any future edit
    /// that silently changes it is caught here, not discovered later
    /// via a mismatched running port.
    #[test]
    fn default_port_is_302_decimal_not_octal() {
        assert_eq!(DEFAULT_API_PORT, 302u16);
        assert_eq!(DEFAULT_API_PORT, 0x12Eu16, "302 decimal == 0x12E");
        assert_ne!(
            DEFAULT_API_PORT, 194u16,
            "302 must never be misread as octal (0o302 == 194 decimal)"
        );
    }

    /// Phase AF evidence: binding the real default produces the exact
    /// documented socket address.
    #[test]
    fn default_port_binds_to_the_documented_socket_when_free() {
        // Best-effort: only meaningful when nothing else on this
        // machine already holds 302. Skips (rather than falsely
        // failing) when it's occupied, since that is a real, separate
        // condition (`falls_back_to_ephemeral_port_on_collision`
        // already covers the fallback behavior itself).
        if TcpListener::bind("127.0.0.1:302").is_err() {
            eprintln!("skipping: 127.0.0.1:302 is already in use on this machine");
            return;
        }
        let listener = bind_loopback(DEFAULT_API_PORT).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.port(), 302);
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(addr.to_string(), "127.0.0.1:302");
    }

    #[test]
    fn binds_preferred_port_when_free() {
        // Bind ephemeral first to get a port we know is free, release
        // it, then ask for that exact port as "preferred".
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);

        let listener = bind_loopback(free_port).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), free_port);
    }

    #[test]
    fn falls_back_to_ephemeral_port_on_collision() {
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let held_port = held.local_addr().unwrap().port();

        let listener = bind_loopback(held_port).unwrap();
        assert_ne!(
            listener.local_addr().unwrap().port(),
            held_port,
            "must not silently reuse the address of a port already held elsewhere"
        );
    }

    #[test]
    fn only_ever_binds_loopback() {
        let listener = bind_loopback(0).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }
    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn default_instance_returns_to_the_canonical_port_when_it_is_free() {
        let canonical = free_port();
        let persisted = free_port();
        let l = bind_for_existing("default", persisted, canonical).unwrap();
        assert_eq!(l.local_addr().unwrap().port(), canonical);
    }

    #[test]
    fn default_instance_keeps_its_persisted_port_when_the_canonical_one_is_busy() {
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let canonical = busy.local_addr().unwrap().port();
        let persisted = free_port();
        let l = bind_for_existing("default", persisted, canonical).unwrap();
        assert_eq!(l.local_addr().unwrap().port(), persisted);
    }

    #[test]
    fn the_default_rule_ignores_ascii_case() {
        for name in ["default", "DEFAULT", "Default"] {
            let canonical = free_port();
            let persisted = free_port();
            let l = bind_for_existing(name, persisted, canonical).unwrap();
            assert_eq!(l.local_addr().unwrap().port(), canonical, "{name}");
        }
    }

    #[test]
    fn other_instances_always_keep_their_persisted_port() {
        let canonical = free_port();
        let persisted = free_port();
        let l = bind_for_existing("reports", persisted, canonical).unwrap();
        assert_eq!(l.local_addr().unwrap().port(), persisted);
    }
}
