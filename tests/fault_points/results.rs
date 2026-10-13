//! Result rows: produced only by executed cases; written as TSV for the matrix document.

use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Row {
    pub class: &'static str,
    pub point: String,
    pub scenario: String,
    pub writers: usize,
    pub trigger: String,
    pub stop: String,
    pub hit: u64,
    pub verdict: &'static str,
    pub distinguishable: String,
    pub snapshot: String,
    pub detail: String,
}

impl Row {
    pub fn tsv(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        [
            self.class.to_string(),
            self.point.clone(),
            self.scenario.clone(),
            self.writers.to_string(),
            self.trigger.clone(),
            self.stop.clone(),
            self.hit.to_string(),
            self.verdict.to_string(),
            self.distinguishable.clone(),
            self.snapshot.clone(),
            self.detail.clone(),
        ]
        .iter()
        .map(|c| clean(c))
        .collect::<Vec<_>>()
        .join("\t")
    }
}

pub fn out_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("fault-points-results");
    fs::create_dir_all(&d).expect("results dir");
    d
}

pub fn write(name: &str, rows: &[Row]) {
    let header = "class\tpoint\tscenario\twriters\ttrigger\tstop\thit\tverdict\tdistinguishable_from_clean_stop\tstate_at_stop\tdetail\n";
    let mut s = String::from(header);
    for r in rows {
        s.push_str(&r.tsv());
        s.push('\n');
    }
    fs::write(out_dir().join(name), s).expect("write results");
}
