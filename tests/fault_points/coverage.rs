//! ADR section 12.1 (A6) and 12.3 (A2): the Point | Status | Reason table is a claim; this test turns it into evidence.
//! * every registered point has exactly one row, every non-alias row names a registered point;
//! * a `Reachable` point must be hit by the workload assigned to it (`scen::scenario_of`), an `Unreachable` point must not
//!   be hit by any workload, an `Alias` must name a registered point;
//! * every `sstable.*` hit carries a non-empty scope label (flush or compaction).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rubixdb::faults::Point;

use crate::results;
use crate::scen::{self, Scen};
use crate::status::{self, Status};

#[test]
fn coverage_matches_adr_status_table() {
    let table = status::table();
    let registered: BTreeSet<String> = Point::ALL.iter().map(|p| p.name().to_string()).collect();
    let mut problems: Vec<String> = Vec::new();

    for name in &registered {
        match table.get(name) {
            None => problems.push(format!("point `{name}` has no row in the ADR status table")),
            Some(r) if r.status == Status::Alias => problems.push(format!(
                "point `{name}` is registered but the table calls it an Alias"
            )),
            _ => {}
        }
    }
    for (name, row) in &table {
        match row.status {
            Status::Alias => {
                let names_a_point = registered
                    .iter()
                    .any(|p| row.reason.contains(&format!("`{p}`")));
                if !names_a_point {
                    problems.push(format!(
                        "alias `{name}`: its reason names no registered point: {}",
                        row.reason
                    ));
                }
            }
            _ => {
                if !registered.contains(name) {
                    problems.push(format!("table row `{name}` is not a registered point"));
                }
            }
        }
    }

    // Traces: one per distinct scenario assigned to a registered point, plus the concurrent engine workload.
    let mut scens: Vec<Scen> = Vec::new();
    for p in Point::ALL {
        if let Some(s) = scen::scenario_of(*p) {
            if !scens.contains(&s) {
                scens.push(s);
            }
        }
    }
    let mut union: BTreeMap<String, u64> = BTreeMap::new();
    let mut per_scen: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut scoped_violations: Vec<String> = Vec::new();
    let mut traces = Vec::new();
    for s in &scens {
        let counts = scen::trace(*s, scen::DEFAULT);
        traces.push((*s, counts));
    }
    let conc = scen::trace(Scen::Engine, scen::CONCURRENT);
    for (s, counts) in traces
        .iter()
        .map(|(s, c)| (s.label(), *c))
        .chain(std::iter::once(("engine(4 writers)".to_string(), conc)))
    {
        for ((point, scope), n) in counts.iter() {
            *union.entry(point.clone()).or_insert(0) += n;
            *per_scen.entry((s.clone(), point.clone())).or_insert(0) += n;
            if point.starts_with("sstable.") && scope == "-" {
                scoped_violations.push(format!(
                    "{s}: {n} hit(s) of `{point}` with an empty scope label"
                ));
            }
        }
    }

    let mut report = String::from("point\tstatus\tscenario\thits_in_its_scenario\thits_anywhere\n");
    for (name, row) in &table {
        if row.status == Status::Alias {
            continue;
        }
        let point = Point::from_name(name);
        let scenario = point.and_then(scen::scenario_of);
        let anywhere = union.get(name).copied().unwrap_or(0);
        let in_own = scenario
            .map(|s| {
                per_scen
                    .get(&(s.label(), name.clone()))
                    .copied()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let _ = writeln!(
            report,
            "{name}\t{:?}\t{}\t{in_own}\t{anywhere}",
            row.status,
            scenario.map(|s| s.label()).unwrap_or_else(|| "-".into())
        );
        match row.status {
            Status::Reachable => match scenario {
                None => problems.push(format!("`{name}` is claimed Reachable but no scenario is assigned to it")),
                Some(s) if in_own == 0 => problems.push(format!(
                    "`{name}` is claimed Reachable by scenario {} but was never hit there (hits anywhere: {anywhere})",
                    s.label()
                )),
                Some(_) => {}
            },
            Status::Unreachable => {
                if anywhere > 0 {
                    problems.push(format!("`{name}` is claimed Unreachable but was hit {anywhere} time(s)"));
                }
                if scenario.is_some() {
                    problems.push(format!("`{name}` is claimed Unreachable but a scenario is assigned to it"));
                }
            }
            Status::Alias => {}
        }
    }
    std::fs::write(results::out_dir().join("coverage.tsv"), &report).unwrap();
    problems.extend(scoped_violations);
    assert!(
        problems.is_empty(),
        "coverage problems:\n  {}",
        problems.join("\n  ")
    );
}
