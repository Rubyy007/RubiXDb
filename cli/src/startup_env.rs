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

/// Reads and validates the environment (every variable of this module, so a bad value stops startup at
/// the same early point whichever variable it is).
pub fn load() -> Result<LocalServerEnv, String> {
    let env = parse(read(RPS_ENV)?.as_deref(), read(BURST_ENV)?.as_deref())?;
    load_max_blocking_threads()?;
    load_allow_truncate_corrupt_wal()?;
    load_sstable_verify_mib_per_sec()?;
    Ok(env)
}

/// `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL` (ADR-WAL-01, F-07): the operator's explicit acceptance of losing the
/// acknowledged WAL records that a clean shutdown recorded but the log no longer holds. Unset or empty = off;
/// exactly `1` = on; anything else stops startup naming the variable (no `true`/`yes`/`01`/padding forms).
/// It bypasses only the `WAL_TAIL_DAMAGED` refusal (and an unwritable quarantine) - never `WAL_CORRUPT`.
pub const ALLOW_TRUNCATE_ENV: &str = rubixdb::ops::wal_tail::OVERRIDE_ENV;

pub fn load_allow_truncate_corrupt_wal() -> Result<bool, String> {
    rubixdb::ops::wal_tail::parse_override(read(ALLOW_TRUNCATE_ENV)?.as_deref())
}

/// `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC` (ADR-SST-01, F-08): the read budget of the background pass that verifies
/// every data block of the tables that were live at start. Digits only; unset or empty = 64 (**an unvalidated
/// default**: its effect on foreground latency under load is unmeasured); `0` = the pass is disabled (`/readyz`
/// `sstable_verification: disabled`); at most 1024. Anything else stops startup naming the variable.
pub const SSTABLE_VERIFY_ENV: &str = "RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC";

pub fn load_sstable_verify_mib_per_sec() -> Result<u64, String> {
    rubixdb::ops::sstable_integrity::parse_verify_mib_per_sec(
        SSTABLE_VERIFY_ENV,
        read(SSTABLE_VERIFY_ENV)?.as_deref(),
    )
}

/// Cap on the embedded server's tokio **blocking thread pool** (`ADR-ITEM-C-01`, `PHASE_ITEM_C_ADR.md`).
/// Every SQL statement runs on that pool (`api/src/routes/sql.rs`); tokio starts a new thread whenever it
/// counts no idle one, and from a cold start under load that created 300-512 threads in 0.1 s and cost
/// throughput (`PHASE_ITEM_C_DISCOVERY.md`). Unset or empty means [`DEFAULT_MAX_BLOCKING_THREADS`], tokio's own
/// default and the behaviour before this setting existed.
///
/// A cap lower than the number of statements running at once makes the excess statements wait for a free
/// thread, and that wait counts toward the statement deadline (`sql_statement_deadline_secs`): choose a cap at
/// or above the expected concurrent statements. The six admin routes share this pool.
pub const BLOCKING_ENV: &str = "RUBIXDB_LOCAL_MAX_BLOCKING_THREADS";
pub const DEFAULT_MAX_BLOCKING_THREADS: usize = 512;
/// Smallest value measured with the real mechanism (`PHASE_ITEM_C_DISCOVERY.md` section 5).
pub const MIN_MAX_BLOCKING_THREADS: usize = 16;
pub const MAX_MAX_BLOCKING_THREADS: usize = 512;

pub fn load_max_blocking_threads() -> Result<usize, String> {
    parse_max_blocking_threads(read(BLOCKING_ENV)?.as_deref())
}

/// Digits only (no sign, no space, no exponent, no radix prefix), within the documented bounds.
pub fn parse_max_blocking_threads(v: Option<&str>) -> Result<usize, String> {
    let Some(v) = v else {
        return Ok(DEFAULT_MAX_BLOCKING_THREADS);
    };
    let bad = || {
        format!(
            "{BLOCKING_ENV} must be an integer between {MIN_MAX_BLOCKING_THREADS} and {MAX_MAX_BLOCKING_THREADS}, got {v:?}"
        )
    };
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n = v.parse::<usize>().map_err(|_| bad())?;
    if !(MIN_MAX_BLOCKING_THREADS..=MAX_MAX_BLOCKING_THREADS).contains(&n) {
        return Err(bad());
    }
    Ok(n)
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

    #[test]
    fn the_wal_tail_override_is_off_by_default_and_on_only_for_exactly_one() {
        assert_eq!(ALLOW_TRUNCATE_ENV, "RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL");
        assert_eq!(rubixdb::ops::wal_tail::parse_override(None), Ok(false));
        assert_eq!(rubixdb::ops::wal_tail::parse_override(Some("")), Ok(false));
        assert_eq!(rubixdb::ops::wal_tail::parse_override(Some("1")), Ok(true));
        for v in [
            "0", "true", "TRUE", "True", "yes", "on", "01", "1.0", " 1", "1 ", "1
", "bogus", "2",
            "-1", "+1", "١",
        ] {
            let err = rubixdb::ops::wal_tail::parse_override(Some(v)).unwrap_err();
            assert!(err.contains(ALLOW_TRUNCATE_ENV), "{v:?}: {err}");
        }
    }

    #[test]
    fn the_sstable_verify_budget_defaults_to_64_zero_disables_and_every_bad_value_names_the_variable(
    ) {
        let p =
            |v| rubixdb::ops::sstable_integrity::parse_verify_mib_per_sec(SSTABLE_VERIFY_ENV, v);
        assert_eq!(
            SSTABLE_VERIFY_ENV,
            "RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC"
        );
        assert_eq!(p(None), Ok(64));
        assert_eq!(p(Some("0")), Ok(0));
        assert_eq!(p(Some("1")), Ok(1));
        assert_eq!(p(Some("1024")), Ok(1024));
        assert_eq!(p(Some("007")), Ok(7));
        for v in [
            "",
            "1025",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1.5",
            "1e2",
            "0x10",
            "abc",
            "64,1",
            "99999999999999999999",
            "١",
        ] {
            let err = p(Some(v)).unwrap_err();
            assert!(err.contains(SSTABLE_VERIFY_ENV), "{v:?}: {err}");
            assert!(err.contains("1024"), "{v:?}: {err}");
        }
    }

    #[test]
    fn the_blocking_pool_cap_defaults_to_the_value_in_force_before_it_existed() {
        // tokio's default `max_blocking_threads` is 512: unset must change nothing.
        assert_eq!(DEFAULT_MAX_BLOCKING_THREADS, 512);
        assert_eq!(parse_max_blocking_threads(None), Ok(512));
    }

    #[test]
    fn the_blocking_pool_cap_accepts_exactly_its_documented_range() {
        for n in [16usize, 17, 32, 64, 256, 511, 512] {
            assert_eq!(parse_max_blocking_threads(Some(&n.to_string())), Ok(n));
        }
        // leading zeros are digits: accepted as the number they spell
        assert_eq!(parse_max_blocking_threads(Some("016")), Ok(16));
    }

    #[test]
    fn every_bad_blocking_pool_cap_is_an_error_naming_the_variable() {
        for v in [
            "",
            "0",
            "1",
            "15",
            "513",
            "1024",
            "abc",
            "-16",
            "+16",
            " 16",
            "16 ",
            "1.5",
            "1e2",
            "0x20",
            "16,32",
            "99999999999999999999",
        ] {
            let err = parse_max_blocking_threads(Some(v)).unwrap_err();
            assert!(err.contains(BLOCKING_ENV), "{v:?}: {err}");
            assert!(err.contains("16") && err.contains("512"), "{v:?}: {err}");
        }
    }
}
