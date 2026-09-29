//! Typed SQL parameter/value JSON encoding — item 6/95/96: parameters
//! and result values stay *typed* across the HTTP boundary, never
//! collapsed to an untyped string a caller would have to re-parse (and
//! never fed back into the SQL text itself — string interpolation is
//! never how a parameter reaches the engine, structurally: this module
//! only ever produces a `rubixdb::relational::RelationalValue`, passed
//! positionally to `rubixdb_sql::exec::{execute, execute_write}`'s own
//! `params: &[Option<RelationalValue>]`, exactly like any other caller
//! of that crate).
//!
//! JSON number precision is the reason every 64-bit-or-wider numeric
//! type (`bigint`, `decimal`'s unscaled part, `time`/`timestamp`'s
//! microsecond count) is carried as a JSON **string**, not a JSON
//! number: JavaScript's `Number` (every browser/Node JSON parser) only
//! represents integers exactly up to 2^53, so a `BIGINT` or `DECIMAL`
//! value beyond that would silently lose precision in transit if sent
//! as a bare JSON number. `date`/`time`/`timestamp` values are ISO 8601
//! text, parsed with `rubixdb_sql::temporal`'s own already-certified
//! parser (never a second date/time parser).

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rubixdb::relational::RelationalValue;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// One typed request parameter, tagged by `type`. Mirrors `Relational
/// Type`'s own variant set exactly (`Boolean`/`Integer`/`Bigint`/`Real`/
/// `Double`/`Decimal`/`Text`/`Blob`/`Date`/`Time`/`Timestamp`) plus
/// `null` for an untyped `NULL` (the binder narrows its type from
/// context, exactly as an untyped `NULL` literal in SQL text already
/// does — item 21 of `PHASE_RELATIONAL_SQL_GRAMMAR.md`, unchanged here).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SqlParam {
    Null,
    Boolean {
        value: bool,
    },
    Integer {
        value: i32,
    },
    /// Carried as a string — see this module's own doc comment.
    Bigint {
        value: String,
    },
    Real {
        value: f32,
    },
    Double {
        value: f64,
    },
    /// `unscaled` is the full-precision integer (e.g. `"12345"` for
    /// `123.45` at `scale: 2`), also a string for the same reason.
    Decimal {
        unscaled: String,
        scale: u8,
    },
    Text {
        value: String,
    },
    /// Base64, the same `_b64` convention `rubixdb-api`'s existing `/v1/
    /// kv` routes already use for raw bytes (`PHASE_API_ARCHITECTURE.md`
    /// §2) — one encoding convention across this whole API, not two.
    Blob {
        value_b64: String,
    },
    /// `"YYYY-MM-DD"`.
    Date {
        value: String,
    },
    /// `"HH:MM:SS"` or `"HH:MM:SS.ffffff"`.
    Time {
        value: String,
    },
    /// `"YYYY-MM-DDTHH:MM:SS"` or with a fractional-seconds suffix.
    Timestamp {
        value: String,
    },
}

impl SqlParam {
    pub fn into_relational_value(self) -> Result<Option<RelationalValue>, ApiError> {
        Ok(match self {
            SqlParam::Null => None,
            SqlParam::Boolean { value } => Some(RelationalValue::Boolean(value)),
            SqlParam::Integer { value } => Some(RelationalValue::Integer(value)),
            SqlParam::Bigint { value } => {
                Some(RelationalValue::Bigint(value.parse::<i64>().map_err(
                    |_| ApiError::Validation(format!("invalid bigint parameter {value:?}")),
                )?))
            }
            SqlParam::Real { value } => Some(RelationalValue::Real(value)),
            SqlParam::Double { value } => Some(RelationalValue::Double(value)),
            SqlParam::Decimal { unscaled, scale } => {
                let v = unscaled.parse::<i128>().map_err(|_| {
                    ApiError::Validation(format!("invalid decimal parameter unscaled={unscaled:?}"))
                })?;
                Some(RelationalValue::Decimal(v, scale))
            }
            SqlParam::Text { value } => Some(RelationalValue::Text(value)),
            SqlParam::Blob { value_b64 } => {
                let bytes = BASE64.decode(&value_b64).map_err(|_| {
                    ApiError::Validation("blob parameter value_b64 is not valid base64".to_string())
                })?;
                Some(RelationalValue::Blob(bytes))
            }
            SqlParam::Date { value } => Some(RelationalValue::Date(
                rubixdb_sql::temporal::parse_date(&value)
                    .map_err(|e| ApiError::Validation(format!("invalid date parameter: {e}")))?,
            )),
            SqlParam::Time { value } => Some(RelationalValue::Time(
                rubixdb_sql::temporal::parse_time(&value)
                    .map_err(|e| ApiError::Validation(format!("invalid time parameter: {e}")))?,
            )),
            SqlParam::Timestamp { value } => Some(RelationalValue::Timestamp(
                rubixdb_sql::temporal::parse_timestamp(&value).map_err(|e| {
                    ApiError::Validation(format!("invalid timestamp parameter: {e}"))
                })?,
            )),
        })
    }
}

/// The typed JSON form of one *result* value — item 96's "preserve
/// column types" applied to the response side. Reuses the identical
/// tag/shape `SqlParam` uses wherever the two are symmetric (a client
/// can echo a result value straight back as a future parameter without
/// reshaping it), plus `null` for a `NULL` result cell.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SqlValueJson {
    Null,
    Boolean { value: bool },
    Integer { value: i32 },
    Bigint { value: String },
    Real { value: f32 },
    Double { value: f64 },
    Decimal { unscaled: String, scale: u8 },
    Text { value: String },
    Blob { value_b64: String },
    Date { value: String },
    Time { value: String },
    Timestamp { value: String },
}

/// `RelationalValue` -> `SqlValueJson`. Never touches display/string
/// formatting for numeric types (`Integer`/`Real`/`Double` stay real
/// JSON numbers, safe within their own precision) — only the types this
/// module's own doc comment names lose JSON-number precision are
/// stringified.
pub fn value_to_json(v: &Option<RelationalValue>) -> SqlValueJson {
    match v {
        None => SqlValueJson::Null,
        Some(RelationalValue::Boolean(b)) => SqlValueJson::Boolean { value: *b },
        Some(RelationalValue::Integer(i)) => SqlValueJson::Integer { value: *i },
        Some(RelationalValue::Bigint(b)) => SqlValueJson::Bigint {
            value: b.to_string(),
        },
        Some(RelationalValue::Real(f)) => SqlValueJson::Real { value: *f },
        Some(RelationalValue::Double(d)) => SqlValueJson::Double { value: *d },
        Some(RelationalValue::Decimal(v, scale)) => SqlValueJson::Decimal {
            unscaled: v.to_string(),
            scale: *scale,
        },
        Some(RelationalValue::Text(s)) => SqlValueJson::Text { value: s.clone() },
        Some(RelationalValue::Blob(b)) => SqlValueJson::Blob {
            value_b64: BASE64.encode(b),
        },
        Some(RelationalValue::Date(d)) => SqlValueJson::Date {
            value: format_date(*d),
        },
        Some(RelationalValue::Time(t)) => SqlValueJson::Time {
            value: format_time(*t),
        },
        Some(RelationalValue::Timestamp(ts)) => SqlValueJson::Timestamp {
            value: format_timestamp(*ts),
        },
    }
}

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Civil-date formatting for a day count since the Unix epoch (the same
/// representation `rubixdb_sql::temporal::parse_date` produces) — the
/// standard days-from-civil algorithm (Howard Hinnant's `civil_from_
/// days`), reused nowhere else in this codebase, so implemented once
/// here rather than pulled in as a dependency for three integer
/// divisions.
fn format_date(days: i32) -> String {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_time(micros: i64) -> String {
    let total_secs = micros.div_euclid(MICROS_PER_SEC);
    let frac = micros.rem_euclid(MICROS_PER_SEC);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:06}")
    }
}

fn format_timestamp(micros: i64) -> String {
    let day = micros.div_euclid(SECS_PER_DAY * MICROS_PER_SEC);
    let time_of_day = micros.rem_euclid(SECS_PER_DAY * MICROS_PER_SEC);
    format!("{}T{}", format_date(day as i32), format_time(time_of_day))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigint_round_trips_through_string_without_precision_loss() {
        let big = i64::MAX;
        let param = SqlParam::Bigint {
            value: big.to_string(),
        };
        let v = param.into_relational_value().unwrap();
        assert_eq!(v, Some(RelationalValue::Bigint(big)));
        let json = value_to_json(&v);
        match json {
            SqlValueJson::Bigint { value } => assert_eq!(value, big.to_string()),
            _ => panic!(),
        }
    }

    #[test]
    fn decimal_round_trips_via_unscaled_and_scale() {
        let param = SqlParam::Decimal {
            unscaled: "12345".to_string(),
            scale: 2,
        };
        let v = param.into_relational_value().unwrap();
        assert_eq!(v, Some(RelationalValue::Decimal(12345, 2)));
    }

    #[test]
    fn blob_round_trips_via_base64() {
        let bytes = vec![0u8, 1, 2, 255, 254];
        let param = SqlParam::Blob {
            value_b64: BASE64.encode(&bytes),
        };
        let v = param.into_relational_value().unwrap();
        assert_eq!(v, Some(RelationalValue::Blob(bytes)));
    }

    #[test]
    fn invalid_bigint_string_is_a_validation_error() {
        let param = SqlParam::Bigint {
            value: "not a number".to_string(),
        };
        assert!(param.into_relational_value().is_err());
    }

    #[test]
    fn date_formatting_matches_known_values() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1), "1970-01-02");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_date(365), "1971-01-01");
    }

    #[test]
    fn date_round_trips_through_parser() {
        for text in [
            "2026-09-29",
            "1969-12-31",
            "2000-02-29",
            "1900-03-01",
            "1600-02-29",
        ] {
            let days = rubixdb_sql::temporal::parse_date(text).unwrap();
            assert_eq!(format_date(days), text, "round-trip mismatch for {text}");
        }
    }

    #[test]
    fn null_param_produces_none() {
        assert_eq!(SqlParam::Null.into_relational_value().unwrap(), None);
    }
}
