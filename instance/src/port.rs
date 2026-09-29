//! Loopback-only port binding with real collision handling -- bind
//! once, keep the exact listener, hand it to the server. No
//! bind-probe-then-rebind TOCTOU window (`PHASE_RUBIXDB_INSTANCE_
//! ARCHITECTURE.md` §5): the caller gets back the live
//! `std::net::TcpListener` it will actually serve on.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener};

pub const DEFAULT_API_PORT: u16 = 8080;

/// Tries `preferred_port` on `127.0.0.1` first (keeps behavior
/// predictable across restarts of the same instance -- a previously
/// persisted `instance.json` gets its old port back whenever nothing
/// else has taken it). On any bind failure (most commonly
/// `AddrInUse`, e.g. two `default` instances racing, or an unrelated
/// process already on 8080) falls back to `127.0.0.1:0`, letting the
/// OS assign a free ephemeral port -- the actually-bound port is what
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
        Err(_) => {
            let ephemeral = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
            TcpListener::bind(ephemeral)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
