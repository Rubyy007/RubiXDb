//! "Did you mean ...?" lines for statements the SQL parser rejected.
//!
//! Only the leading keywords of the caller's own statement are looked at, and only keywords from a small
//! closed list are ever echoed back: the hint can never contain anything but those keywords.

const STARTERS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "USE", "EXPLAIN", "BEGIN", "COMMIT",
    "ROLLBACK", "WITH", "START",
];
const CREATE_TARGETS: &[&str] = &["TABLE", "INDEX", "UNIQUE", "SCHEMA", "DATABASE"];
const DROP_TARGETS: &[&str] = &["TABLE", "INDEX"];
const USE_TARGETS: &[&str] = &["SCHEMA"];

/// Classic Levenshtein distance on ASCII upper-case words (both inputs are short, bounded by the caller).
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<u8> = a.bytes().collect();
    let b: Vec<u8> = b.bytes().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest candidate within a distance that scales with the word (at most 2), or `None`.
fn closest<'a>(word: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if word.len() < 3 {
        return None;
    }
    let limit = if word.len() <= 4 { 1 } else { 2 };
    candidates
        .iter()
        .map(|c| (distance(word, c), *c))
        .filter(|(d, _)| *d > 0 && *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn leading_words(sql: &str) -> Vec<String> {
    sql.split_whitespace()
        .take(3)
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_ascii_uppercase()
        })
        .collect()
}

pub fn statement_hint(sql: &str) -> Option<String> {
    let words = leading_words(sql);
    let first = words.first()?.as_str();
    if first.is_empty() {
        return None;
    }
    match first {
        "ALTER" | "TRUNCATE" | "GRANT" | "REVOKE" => {
            return Some(format!("{first} is not supported."));
        }
        "SHOW" | "DESCRIBE" | "DESC" | "LIST" => {
            return Some(
                r"There is no SHOW or DESCRIBE statement. The CLI has \ls (schemas), \lt (tables) and \d <table>; the console has the Schema and Tables lists."
                    .to_string(),
            );
        }
        _ => {}
    }
    if !STARTERS.contains(&first) {
        return closest(first, STARTERS).map(|s| format!("Did you mean {s}?"));
    }
    let second = words.get(1).map(String::as_str).unwrap_or("");
    let targets: &[&str] = match first {
        "CREATE" => CREATE_TARGETS,
        "DROP" => DROP_TARGETS,
        "USE" => USE_TARGETS,
        _ => return None,
    };
    if second.is_empty() || targets.contains(&second) {
        return None;
    }
    if first == "DROP" && (second == "SCHEMA" || second == "DATABASE") {
        return Some(format!(
            "DROP {second} is not available as a SQL statement."
        ));
    }
    match closest(second, targets) {
        Some(t) => Some(format!("Did you mean {first} {t}?")),
        None => Some(format!(
            "{first} can be followed by {}.",
            targets.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mistyped_first_keyword() {
        assert_eq!(
            statement_hint("SELCT * FROM t").as_deref(),
            Some("Did you mean SELECT?")
        );
        assert_eq!(
            statement_hint("creat table t (id integer)").as_deref(),
            Some("Did you mean CREATE?")
        );
        assert_eq!(
            statement_hint("INSRT INTO t VALUES (1)").as_deref(),
            Some("Did you mean INSERT?")
        );
    }

    #[test]
    fn mistyped_second_keyword() {
        assert_eq!(
            statement_hint("CREATE SCEMA x").as_deref(),
            Some("Did you mean CREATE SCHEMA?")
        );
        assert_eq!(
            statement_hint("create databse d").as_deref(),
            Some("Did you mean CREATE DATABASE?")
        );
        assert_eq!(
            statement_hint("USE SCHEM x").as_deref(),
            Some("Did you mean USE SCHEMA?")
        );
        assert_eq!(
            statement_hint("DROP TABEL t").as_deref(),
            Some("Did you mean DROP TABLE?")
        );
    }

    #[test]
    fn unrelated_words_get_the_list_or_nothing() {
        assert_eq!(
            statement_hint("CREATE WIDGET x").as_deref(),
            Some("CREATE can be followed by TABLE, INDEX, UNIQUE, SCHEMA, DATABASE.")
        );
        assert_eq!(statement_hint("xyzzy plugh"), None);
        assert_eq!(statement_hint(""), None);
        assert_eq!(statement_hint("   ;  "), None);
    }

    #[test]
    fn valid_statements_with_other_errors_get_no_keyword_hint() {
        assert_eq!(statement_hint("SELECT FROM"), None);
        assert_eq!(statement_hint("CREATE TABLE"), None);
    }

    #[test]
    fn known_unsupported_statements_say_so() {
        assert_eq!(
            statement_hint("ALTER TABLE t ADD c int").as_deref(),
            Some("ALTER is not supported.")
        );
        assert!(statement_hint("SHOW TABLES").unwrap().contains(r"\lt"));
        assert_eq!(
            statement_hint("DROP SCHEMA s").as_deref(),
            Some("DROP SCHEMA is not available as a SQL statement.")
        );
    }

    #[test]
    fn the_hint_only_ever_contains_listed_keywords() {
        let h = statement_hint("selct '; DROP TABLE users; --").unwrap();
        assert_eq!(h, "Did you mean SELECT?");
    }
}
