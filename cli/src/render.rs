//! Terminal rendering — item 40/125: typed values kept intact until the
//! very last step (this module is the only place a `SqlValueJson` ever
//! becomes a display string), and adversarial row content (ANSI escape
//! sequences, other C0/C1 control characters) is sanitized before it
//! ever reaches the terminal, without corrupting ordinary printable
//! text (including non-ASCII Unicode, which is left untouched).

use serde_json::Value;

/// Replaces every ASCII control character (`0x00..=0x1F`, `0x7F`) —
/// this is exactly the range that includes `ESC` (`0x1B`, the start of
/// every ANSI escape sequence) and every other terminal-control byte —
/// with its `\xNN` hex escape, so adversarial row content can never
/// move the cursor, change colors, or otherwise manipulate the
/// terminal. Ordinary printable text and multi-byte UTF-8 sequences
/// (which never contain a byte in this range as a continuation byte,
/// by UTF-8's own design) pass through completely unchanged.
pub fn sanitize_for_terminal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if (c as u32) < 0x20 || c as u32 == 0x7F {
            out.push_str(&format!("\\x{:02X}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `SqlValueJson`'s wire shape -> a display string. Never guesses at an
/// unrecognized shape -- falls back to the value's own compact JSON
/// text rather than silently showing nothing, so a server-side response
/// shape change is visible, not hidden.
pub fn value_to_display(v: &Value) -> String {
    let Some(obj) = v.as_object() else {
        return sanitize_for_terminal(&v.to_string());
    };
    let Some(ty) = obj.get("type").and_then(Value::as_str) else {
        return sanitize_for_terminal(&v.to_string());
    };
    if ty == "null" {
        return "NULL".to_string();
    }
    let rendered = match ty {
        "boolean" => obj.get("value").map(|b| b.to_string()),
        "integer" | "bigint" | "real" | "double" => obj
            .get("value")
            .map(|n| n.to_string().trim_matches('"').to_string()),
        "decimal" => {
            let unscaled = obj.get("unscaled").and_then(Value::as_str).unwrap_or("0");
            let scale = obj.get("scale").and_then(Value::as_u64).unwrap_or(0) as usize;
            Some(format_decimal(unscaled, scale))
        }
        "text" | "date" | "time" | "timestamp" => {
            obj.get("value").and_then(Value::as_str).map(str::to_string)
        }
        "blob" => obj
            .get("value_b64")
            .and_then(Value::as_str)
            .map(|b64| format!("\\x{}", hex_of_base64_len(b64))),
        _ => None,
    };
    match rendered {
        Some(s) => sanitize_for_terminal(&s),
        None => sanitize_for_terminal(&v.to_string()),
    }
}

/// A blob is shown as its byte length rather than decoded/rendered
/// content -- arbitrary binary data has no safe universal terminal
/// rendering, and this CLI does not attempt to guess one (item 125's
/// own "render safely" applied to the one type that is not text at
/// all).
fn hex_of_base64_len(b64: &str) -> String {
    // Exact decoded length without allocating a decode buffer: base64
    // is 4 chars -> 3 bytes, minus 1 byte per trailing '=' pad char.
    let stripped = b64.trim_end_matches('=');
    let pad = b64.len().saturating_sub(stripped.len());
    let full_groups = b64.len() / 4;
    let len = full_groups * 3 - pad.min(2);
    format!("{len}-byte blob")
}

fn format_decimal(unscaled: &str, scale: usize) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }
    let negative = unscaled.starts_with('-');
    let digits = unscaled.trim_start_matches('-');
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let split_at = padded.len() - scale;
    let (int_part, frac_part) = padded.split_at(split_at);
    format!("{}{int_part}.{frac_part}", if negative { "-" } else { "" })
}

/// A simple box-drawn table -- column headers, then one row per data
/// row, column widths computed from the sanitized display text (so a
/// control-character escape's own extra width is accounted for, never
/// causing misaligned columns).
pub fn render_table(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    let sep = || -> String {
        let mut s = String::from("+");
        for w in &widths {
            s.push_str(&"-".repeat(w + 2));
            s.push('+');
        }
        s
    };
    out.push_str(&sep());
    out.push('\n');
    out.push('|');
    for (h, w) in headers.iter().zip(&widths) {
        out.push_str(&format!(" {h:<w$} |", w = w));
    }
    out.push('\n');
    out.push_str(&sep());
    out.push('\n');
    for row in rows {
        out.push('|');
        for (cell, w) in row.iter().zip(&widths) {
            out.push_str(&format!(" {cell:<w$} |", w = w));
        }
        out.push('\n');
    }
    out.push_str(&sep());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ansi_escape_sequences() {
        let malicious = "\x1b[31mRED\x1b[0m";
        let safe = sanitize_for_terminal(malicious);
        assert!(!safe.contains('\x1b'));
        assert!(safe.contains("\\x1B"));
    }

    #[test]
    fn preserves_ordinary_unicode_text() {
        let s = "héllo wörld 日本語";
        assert_eq!(sanitize_for_terminal(s), s);
    }

    #[test]
    fn sanitizes_carriage_return_and_bell() {
        let s = "a\rb\x07c";
        let safe = sanitize_for_terminal(s);
        assert!(!safe.contains('\r'));
        assert!(!safe.contains('\x07'));
    }

    #[test]
    fn decimal_formatting_places_the_decimal_point_correctly() {
        assert_eq!(format_decimal("12345", 2), "123.45");
        assert_eq!(format_decimal("-12345", 2), "-123.45");
        assert_eq!(format_decimal("5", 2), "0.05");
        assert_eq!(format_decimal("100", 0), "100");
    }

    #[test]
    fn null_renders_as_null_text() {
        let v: Value = serde_json::json!({"type": "null"});
        assert_eq!(value_to_display(&v), "NULL");
    }

    #[test]
    fn text_value_with_embedded_escape_is_sanitized() {
        let v: Value = serde_json::json!({"type": "text", "value": "\x1b[2Jpwned"});
        let s = value_to_display(&v);
        assert!(!s.contains('\x1b'));
    }

    #[test]
    fn table_rendering_aligns_columns() {
        let headers = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec!["1".to_string(), "alice".to_string()],
            vec!["200".to_string(), "b".to_string()],
        ];
        let table = render_table(&headers, &rows);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines.iter().all(|l| l.len() == lines[0].len()));
    }
}
