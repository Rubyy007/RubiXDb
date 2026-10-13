//! Reads the `Point | Status | Reason` table of ADR-FAULTPOINTS-01 section 12.1 (between the two marker comments).

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Reachable,
    Unreachable,
    Alias,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub status: Status,
    pub reason: String,
}

pub fn adr_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("adr")
        .join("ADR-FAULTPOINTS-01.md")
}

pub fn table() -> BTreeMap<String, Row> {
    let text = std::fs::read_to_string(adr_path()).expect("ADR file");
    let begin = text
        .find("<!-- POINT-STATUS-BEGIN -->")
        .expect("begin marker");
    let end = text.find("<!-- POINT-STATUS-END -->").expect("end marker");
    let mut out = BTreeMap::new();
    for line in text[begin..end].lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        // | `name` | Status | Reason |
        if cells.len() < 5 || !cells[1].starts_with('`') {
            continue;
        }
        let name = cells[1].trim_matches('`').to_string();
        let status = match cells[2] {
            "Reachable" => Status::Reachable,
            "Unreachable" => Status::Unreachable,
            "Alias" => Status::Alias,
            other => panic!("ADR table: row {name}: unknown status {other:?}"),
        };
        let reason = cells[3..cells.len() - 1].join("|");
        assert!(
            out.insert(name.clone(), Row { status, reason }).is_none(),
            "duplicate row {name}"
        );
    }
    assert!(!out.is_empty(), "the ADR status table is empty");
    out
}
