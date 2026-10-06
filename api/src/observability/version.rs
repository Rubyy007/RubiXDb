//! Product version and build identity. Nothing here is guessed: the revision is whatever
//! `build.rs` could read from git at build time (otherwise `null`), and no local path, user name
//! or host name is ever included.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub const PRODUCT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `RUBIXDB_GIT_REVISION` as set by `build.rs`, if it could be determined.
pub fn git_revision() -> Option<&'static str> {
    option_env!("RUBIXDB_GIT_REVISION")
}

/// `<version>-<profile>-<arch>-<os>`, e.g. `0.1.0-release-x86_64-windows`: what kind of build
/// this is, without any path.
pub fn build_identifier() -> String {
    format!(
        "{}-{}-{}-{}",
        PRODUCT_VERSION,
        option_env!("RUBIXDB_BUILD_PROFILE").unwrap_or("unknown"),
        std::env::consts::ARCH,
        std::env::consts::OS
    )
}

pub fn info(started_at: SystemTime) -> Value {
    json!({
        "product_version": PRODUCT_VERSION,
        "build_identifier": build_identifier(),
        "git_revision": git_revision(),
        "startup_timestamp_unix_ms": started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_has_no_paths_and_a_null_revision_is_allowed() {
        let b = build_identifier();
        assert!(b.starts_with(PRODUCT_VERSION));
        assert!(!b.contains('\\') && !b.contains('/') && !b.contains(':'));
        if let Some(r) = git_revision() {
            let base = r.strip_suffix("-dirty").unwrap_or(r);
            assert!(base.bytes().all(|c| c.is_ascii_hexdigit()), "{r}");
        }
    }
}
