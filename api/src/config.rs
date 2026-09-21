//! Configuration loading — `PHASE_API_ARCHITECTURE.md` §6. Every
//! setting is externalized via environment variables; nothing is
//! hardcoded, and no default API key is ever valid (there is no
//! default — `RUBIXDB_API_KEYS` is required).

use std::env;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Reader,
    Admin,
}

impl Role {
    /// `Admin` can do everything `Reader` can (role hierarchy, not two
    /// disjoint sets) — `PHASE_API_ARCHITECTURE.md` §4.
    pub fn satisfies(self, required: Role) -> bool {
        match required {
            Role::Reader => true, // both Reader and Admin satisfy a Reader requirement
            Role::Admin => self == Role::Admin,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApiKeyConfig {
    pub name: String,
    pub role: Role,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen_addr: SocketAddr,
    pub api_keys: Vec<ApiKeyConfig>,
    pub max_value_bytes: usize,
    pub max_key_bytes: usize,
    pub default_range_limit: usize,
    pub max_range_limit: usize,
    pub shutdown_drain_secs: u64,
    pub rate_limit_rps: f64,
    pub rate_limit_burst: u32,
    pub compaction_auto_trigger: bool,
    pub compaction_trigger_count: usize,
    /// Origins allowed to make cross-origin requests (`RUBIXDB_CORS_
    /// ALLOWED_ORIGINS`, comma-separated, e.g.
    /// `https://console.example.com`) -- empty by default (no CORS
    /// headers at all, same-origin only), the safest default for a
    /// mutable admin API. A separately-hosted frontend (the normal
    /// deployment shape for this console -- `PHASE_FRONTEND_
    /// ARCHITECTURE.md`) must set this explicitly; it is never
    /// wildcarded (`*`) regardless of configuration, since credentials
    /// (the `Authorization` header) are in play.
    pub cors_allowed_origins: Vec<String>,
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration error: {}", self.0)
    }
}
impl std::error::Error for ConfigError {}

fn env_var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> Result<T, ConfigError> {
    match env_var(name) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| ConfigError(format!("{name} is not a valid value: {v:?}"))),
    }
}

/// Parses `RUBIXDB_API_KEYS`, format: `name:role:key,name:role:key,...`
/// — `role` is `reader` or `admin` (case-insensitive). At least one
/// `admin` key is required (otherwise no client could ever write).
fn parse_api_keys(raw: &str) -> Result<Vec<ApiKeyConfig>, ConfigError> {
    let mut keys = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.splitn(3, ':').collect();
        let [name, role_str, key] = parts.as_slice() else {
            return Err(ConfigError(format!(
                "RUBIXDB_API_KEYS entry {entry:?} must be name:role:key"
            )));
        };
        let role = match role_str.to_ascii_lowercase().as_str() {
            "reader" => Role::Reader,
            "admin" => Role::Admin,
            other => {
                return Err(ConfigError(format!(
                    "RUBIXDB_API_KEYS entry {entry:?}: unknown role {other:?} (expected reader or admin)"
                )))
            }
        };
        if key.len() < 16 {
            return Err(ConfigError(format!(
                "RUBIXDB_API_KEYS entry for {name:?}: key must be at least 16 characters"
            )));
        }
        keys.push(ApiKeyConfig {
            name: name.to_string(),
            role,
            key: key.to_string(),
        });
    }
    if keys.is_empty() {
        return Err(ConfigError(
            "RUBIXDB_API_KEYS must contain at least one entry".to_string(),
        ));
    }
    if !keys.iter().any(|k| k.role == Role::Admin) {
        return Err(ConfigError(
            "RUBIXDB_API_KEYS must contain at least one admin-role key".to_string(),
        ));
    }
    Ok(keys)
}

impl Config {
    /// Deterministic, fail-fast load — `PHASE_API_ARCHITECTURE.md` §6:
    /// startup must fail loudly on any misconfiguration, never silently
    /// fall back to an insecure or ambiguous default.
    pub fn load_from_env() -> Result<Self, ConfigError> {
        let data_dir = env_var("RUBIXDB_DATA_DIR")
            .ok_or_else(|| ConfigError("RUBIXDB_DATA_DIR is required".to_string()))?;
        let listen_addr: SocketAddr = env_or("RUBIXDB_LISTEN_ADDR", "127.0.0.1:8080".to_string())?
            .parse()
            .map_err(|e| ConfigError(format!("RUBIXDB_LISTEN_ADDR invalid: {e}")))?;
        let api_keys_raw = env_var("RUBIXDB_API_KEYS")
            .ok_or_else(|| ConfigError("RUBIXDB_API_KEYS is required".to_string()))?;
        let api_keys = parse_api_keys(&api_keys_raw)?;

        Ok(Config {
            data_dir: PathBuf::from(data_dir),
            listen_addr,
            api_keys,
            max_value_bytes: env_or("RUBIXDB_MAX_VALUE_BYTES", 1024 * 1024)?,
            max_key_bytes: env_or("RUBIXDB_MAX_KEY_BYTES", 4096)?,
            default_range_limit: env_or("RUBIXDB_DEFAULT_RANGE_LIMIT", 100)?,
            max_range_limit: env_or("RUBIXDB_MAX_RANGE_LIMIT", 10_000)?,
            shutdown_drain_secs: env_or("RUBIXDB_SHUTDOWN_DRAIN_SECS", 30)?,
            rate_limit_rps: env_or("RUBIXDB_RATE_LIMIT_RPS", 50.0)?,
            rate_limit_burst: env_or("RUBIXDB_RATE_LIMIT_BURST", 100)?,
            // Deliberately different from the raw engine's own
            // conservative `LsmConfig::default()` (`compaction_auto_
            // trigger: false`, chosen to protect Increment 1-era test
            // fixtures that predate Compaction, per `ADR-COMPACTION-001`
            // Amendment 1 §A5) -- this product's own deployment default
            // has no such legacy-test-surface concern, and an operator
            // running a real service expects automatic housekeeping by
            // default. Still fully overridable per-deployment.
            compaction_auto_trigger: env_or("RUBIXDB_COMPACTION_AUTO_TRIGGER", true)?,
            compaction_trigger_count: env_or("RUBIXDB_COMPACTION_TRIGGER_COUNT", 4)?,
            cors_allowed_origins: env_var("RUBIXDB_CORS_ALLOWED_ORIGINS")
                .map(|raw| {
                    raw.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_api_keys() {
        let keys =
            parse_api_keys("svc-a:admin:0123456789abcdef,svc-b:reader:fedcba9876543210").unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].name, "svc-a");
        assert_eq!(keys[0].role, Role::Admin);
        assert_eq!(keys[1].role, Role::Reader);
    }

    #[test]
    fn rejects_missing_admin_key() {
        let err = parse_api_keys("svc-b:reader:fedcba9876543210").unwrap_err();
        assert!(err.0.contains("admin"));
    }

    #[test]
    fn rejects_short_key() {
        let err = parse_api_keys("svc-a:admin:short").unwrap_err();
        assert!(err.0.contains("16 characters"));
    }

    #[test]
    fn rejects_empty_key_list() {
        let err = parse_api_keys("").unwrap_err();
        assert!(err.0.contains("at least one entry"));
    }

    #[test]
    fn role_hierarchy_admin_satisfies_reader_requirement() {
        assert!(Role::Admin.satisfies(Role::Reader));
        assert!(Role::Admin.satisfies(Role::Admin));
        assert!(Role::Reader.satisfies(Role::Reader));
        assert!(!Role::Reader.satisfies(Role::Admin));
    }
}
