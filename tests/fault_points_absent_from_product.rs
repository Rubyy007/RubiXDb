//! ADR-FAULTPOINTS-01 section 12.7 (A7): the `fault-points` feature must not ship. This is the workspace-test layer of the
//! guard (the other layer is the release gate in `scripts/release.ps1`, which byte-searches the built `rubixdb.exe`).
//!
//! Not feature-gated: it runs in the default `cargo test --workspace`. It does not touch `tests/repo_hygiene.rs`.

use std::fs;
use std::path::{Path, PathBuf};

const FEATURE: &str = "fault-points";
/// D1b (ADR-PERSISTENCE-TRAIT-01): the power-loss simulator behind `src/persist` is held to the same rule as `fault-points`.
const SIM_FEATURE: &str = "sim-fs";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".vs"
            | "dist"
            | "dist-release"
            | "__pycache__"
            | ".long-endurance-data"
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if p.is_dir() {
            if !skip_dir(&name) {
                walk(&p, out);
            }
        } else {
            out.push(p);
        }
    }
}

/// (i) No workspace manifest other than the root names the feature at all; the root names it only as its own feature
/// definition, in comments, and in `required-features` of test/example targets - never as a dependency feature.
#[test]
fn no_cargo_manifest_enables_fault_points_for_a_dependency() {
    let root = repo_root();
    let mut manifests = vec![root.join("Cargo.toml")];
    for member in ["api", "sql", "cli", "instance"] {
        manifests.push(root.join(member).join("Cargo.toml"));
    }
    for m in &manifests {
        let text = read(m);
        for feature in [FEATURE, SIM_FEATURE] {
            for (n, line) in text.lines().enumerate() {
                if !line.contains(feature) {
                    continue;
                }
                let t = line.trim();
                let is_root = m == &root.join("Cargo.toml");
                let allowed = is_root
                    && (t.starts_with('#')
                        || t == format!("{feature} = []")
                        || t.starts_with("required-features"));
                assert!(
                    allowed,
                    "{}:{}: the `{feature}` feature appears outside the root package's own feature definition / required-features: {t:?}",
                    m.display(),
                    n + 1
                );
            }
        }
    }
}

/// The guard's predicate, separate so that it can be shown to fail: a command line that builds, installs, runs, packages or
/// publishes with the feature or with `--all-features`, and does not name a rig target.
fn builds_a_shipping_artefact_with_the_feature(l: &str) -> bool {
    let shipping = [
        "cargo build",
        "cargo install",
        "cargo run",
        "cargo rustc",
        "cargo package",
        "cargo publish",
    ]
    .iter()
    .any(|c| l.contains(c));
    let rig_target = [
        "--example",
        "--examples",
        "--test",
        "--tests",
        "--bench",
        "--benches",
    ]
    .iter()
    .any(|t| l.contains(t));
    shipping
        && !rig_target
        && (l.contains(FEATURE) || l.contains(SIM_FEATURE) || l.contains("--all-features"))
}

/// The guard must be able to fail: each of these command lines is a violation or is allowed, as stated.
#[test]
fn the_shipping_guard_flags_what_it_must_and_allows_what_it_must() {
    for bad in [
        "cargo build --release --locked -p rubixdb-cli --features rubixdb/fault-points",
        "cargo build --release --all-features",
        "cargo install --path cli --features fault-points",
        "cargo run --release --features fault-points",
        "cargo publish --all-features",
        "cargo build --release -p rubixdb-cli --features rubixdb/sim-fs",
        "cargo install --path cli --features sim-fs",
    ] {
        assert!(
            builds_a_shipping_artefact_with_the_feature(bad),
            "not flagged: {bad}"
        );
    }
    for ok in [
        "cargo build --features fault-points --example fault_child",
        "cargo build --features fault-points,sim-fs --example fault_child",
        "cargo build --release --locked -p rubixdb-cli",
        "cargo clippy --workspace --all-targets --all-features -- -D warnings",
        "cargo test --features fault-points --test fault_points",
        "cargo check --all-features",
    ] {
        assert!(
            !builds_a_shipping_artefact_with_the_feature(ok),
            "wrongly flagged: {ok}"
        );
    }
}

/// (ii) No script runs a shipping-style cargo command (`build`, `install`, `run`, `rustc`, `package`, `publish`) with the
/// feature or with `--all-features`. `check`, `clippy`, `test`, `doc` are not shipping artefacts and are allowed to use
/// `--all-features` (the release script's clippy step does). A command that names an `--example`, `--test` or `--bench` target
/// builds a rig artefact, not the product binary, and is allowed: the first run of this guard flagged the documented command
/// that builds the kill-9 rig's own child (`cargo build --features fault-points --example fault_child`), which is exactly such
/// a case (ADR-FAULTPOINTS-01 section 12.7, refinement recorded when the guard first ran).
#[test]
fn no_script_builds_a_shipping_artefact_with_fault_points_or_all_features() {
    let root = repo_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let mut checked = 0;
    for p in files {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "ps1" | "sh" | "py" | "bat" | "cmd" | "yml" | "yaml") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        checked += 1;
        for (n, line) in text.lines().enumerate() {
            let l = line.trim();
            if l.starts_with('#') {
                continue;
            }
            if builds_a_shipping_artefact_with_the_feature(l) {
                panic!(
                    "{}:{}: shipping-style cargo command uses `{FEATURE}`, `{SIM_FEATURE}` or `--all-features`: {l:?}",
                    p.display(),
                    n + 1
                );
            }
        }
    }
    assert!(
        checked > 0,
        "the scan found no scripts at all: the scan itself is broken"
    );
}

/// (iii) `src/faults.rs` reads no environment variable outside the feature-gated module, so a product build contains none
/// of the control variables.
#[test]
fn faults_module_reads_the_environment_only_inside_the_gated_module() {
    let text = read(&repo_root().join("src").join("faults.rs"));
    let gated = text
        .find("#[cfg(feature = \"fault-points\")]\nmod imp")
        .or_else(|| text.find("#[cfg(feature = \"fault-points\")]\r\nmod imp"))
        .expect("the gated `mod imp` is missing");
    for needle in ["env::var", "env::args", "var_os"] {
        if let Some(i) = text.find(needle) {
            assert!(
                i > gated,
                "`{needle}` appears in src/faults.rs before the feature-gated module (byte {i} < {gated})"
            );
        }
    }
}

/// The simulator module exists only behind `sim-fs` in `src/persist`, and no other workspace crate names it.
#[test]
fn sim_fs_is_gated_in_persist_and_named_nowhere_else() {
    let root = repo_root();
    let text = read(&root.join("src").join("persist").join("mod.rs"));
    let at = text
        .find("pub mod sim;")
        .expect("`pub mod sim;` missing from src/persist/mod.rs");
    let last_line = text[..at].trim_end().lines().last().unwrap_or("");
    assert!(
        last_line.trim() == "#[cfg(feature = \"sim-fs\")]",
        "`pub mod sim;` is not directly preceded by #[cfg(feature = \"sim-fs\")] (found {last_line:?})"
    );
    for member in ["api", "sql", "cli", "instance"] {
        let mut files = Vec::new();
        walk(&root.join(member), &mut files);
        for p in files {
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let t = fs::read_to_string(&p).unwrap_or_default();
            assert!(
                !t.contains("feature = \"sim-fs\"") && !t.contains("persist::sim"),
                "{} names the simulator, which only the root package's tests and examples may",
                p.display()
            );
        }
    }
}

/// No other workspace crate gates code on the feature (it would be a cfg on a feature those crates do not define).
#[test]
fn no_other_crate_cfgs_on_the_feature() {
    let root = repo_root();
    for member in ["api", "sql", "cli", "instance"] {
        let mut files = Vec::new();
        walk(&root.join(member), &mut files);
        for p in files {
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&p).unwrap_or_default();
            assert!(
                !text.contains("feature = \"fault-points\""),
                "{} cfgs on `fault-points`, which only the root package defines",
                p.display()
            );
        }
    }
}

/// (iv) The release script carries the binary byte-search gate for every control variable.
#[test]
fn release_script_has_the_binary_search_gate() {
    let release = read(&repo_root().join("scripts").join("release.ps1"));
    assert!(
        release.contains("check_no_fault_points.ps1"),
        "scripts/release.ps1 does not run the fault-point binary gate"
    );
    let text = read(
        &repo_root()
            .join("scripts")
            .join("check_no_fault_points.ps1"),
    );
    for var in [
        "RUBIXDB_FAULT_SPEC",
        "RUBIXDB_FAULT_LOG",
        "RUBIXDB_FAULT_TRACE",
        "RUBIXDB_FAULT_READY",
        "RBXSIMD1",
    ] {
        assert!(
            text.contains(var),
            "scripts/check_no_fault_points.ps1 does not search the binary for {var}"
        );
    }
}
