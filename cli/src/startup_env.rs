//! Externally supplied configuration of the local (embedded) server, parsed
//! **strictly** and **before** anything on disk is created or any lock/socket is
//! taken. An unset or empty variable means "use the documented default"; any
//! other value must be valid, otherwise startup fails with a message naming the
//! variable -- there is no silent fallback to a default.
//!
//! Documented defaults (a local instance has exactly one principal, so the
//! limiter is sized far above measured throughput; see `host.rs`).

pub const RPS_ENV: &str = "RUBIXDB_LOCAL_RATE_LIMIT_RPS";
pub const BURST_ENV: &str = "RUBIXDB_LOCAL_RATE_LIMIT_BURST";
pub const DEFAULT_RATE_LIMIT_RPS: f64 = 100_000.0;
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 200_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalServerEnv {
    pub rate_limit_rps: f64,
    pub rate_limit_burst: u32,
}

fn read(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(v) if v.is_empty() => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
    }
}

/// Reads and validates the environment.
pub fn load() -> Result<LocalServerEnv, String> {
    parse(read(RPS_ENV)?.as_deref(), read(BURST_ENV)?.as_deref())
}

pub fn parse(rps: Option<&str>, burst: Option<&str>) -> Result<LocalServerEnv, String> {
    let rate_limit_rps = match rps {
        None => DEFAULT_RATE_LIMIT_RPS,
        Some(v) => v
            .parse::<f64>()
            .map_err(|_| format!("{RPS_ENV} must be a number greater than 0, got {v:?}"))?,
    };
    let rate_limit_burst = match burst {
        None => DEFAULT_RATE_LIMIT_BURST,
        Some(v) => v.parse::<u32>().map_err(|_| {
            format!(
                "{BURST_ENV} must be an integer between 1 and {}, got {v:?}",
                u32::MAX
            )
        })?,
    };
    // Validate each value on its own so the message names the right variable.
    rubixdb_api::config::validate_rate_limit(rate_limit_rps, 1)
        .map_err(|why| format!("{RPS_ENV}: {why}"))?;
    rubixdb_api::config::validate_rate_limit(1.0, rate_limit_burst)
        .map_err(|why| format!("{BURST_ENV}: {why}"))?;
    Ok(LocalServerEnv {
        rate_limit_rps,
        rate_limit_burst,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_the_documented_defaults() {
        let e = parse(None, None).unwrap();
        assert_eq!(e.rate_limit_rps, DEFAULT_RATE_LIMIT_RPS);
        assert_eq!(e.rate_limit_burst, DEFAULT_RATE_LIMIT_BURST);
    }

    #[test]
    fn valid_values_are_used() {
        let e = parse(Some("500"), Some("1000")).unwrap();
        assert_eq!((e.rate_limit_rps, e.rate_limit_burst), (500.0, 1000));
        assert!(parse(Some("0.5"), Some("1")).is_ok());
        assert!(parse(Some("1e3"), None).is_ok());
    }

    #[test]
    fn every_bad_value_is_an_error_naming_the_variable() {
        for rps in [
            "",
            " 5",
            "abc",
            "0",
            "-5",
            "NaN",
            "inf",
            "-inf",
            "1e999",
            "2000000000",
        ] {
            if rps.is_empty() {
                continue; // empty is "unset" at the environment layer, not here
            }
            let err = parse(Some(rps), None).unwrap_err();
            assert!(err.contains(RPS_ENV), "{rps:?}: {err}");
        }
        for burst in [
            "abc",
            "-1",
            "0",
            "4294967296",
            "1.5",
            " 5",
            "99999999999999999999",
        ] {
            let err = parse(None, Some(burst)).unwrap_err();
            assert!(err.contains(BURST_ENV), "{burst:?}: {err}");
        }
    }
}
