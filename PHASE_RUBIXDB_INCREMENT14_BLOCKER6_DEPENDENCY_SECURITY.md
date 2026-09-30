# Increment 14, Blocker 6 — Dependency Advisory Scan

Real tool, real database, real result — not inferred from compilation.

## 1. Tool

`cargo-audit 0.22.2`, installed this increment (`cargo install cargo-audit
--locked`; was not present before — Increment 13's own security record
named this as the one item it could not close). RustSec's own official
scanner, run against the workspace's real, checked-in `Cargo.lock`.

## 2. Command and real output

```
$ cargo audit
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 1277 security advisories (from C:\Users\Ruby\.cargo\advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (252 crate dependencies)
$ echo EXIT=$?
EXIT=0
```

No advisory table was printed (cargo-audit only prints a table and
returns a non-zero exit code when it finds a match); exit code `0`
confirms zero matches. Reproduced twice, including with
`--color never` and a raw byte dump (`cat -A`) to rule out hidden/
truncated output — both runs end at the "Scanning..." line with
nothing further.

## 3. Scope of what this actually checked

- **1,277 real advisories** from the official RustSec advisory
  database (freshly fetched, not a stale local cache — the `Fetching`
  step ran live against `github.com/RustSec/advisory-db`).
- **252 crate dependencies** — the workspace's full resolved dependency
  graph (`Cargo.lock`), direct and transitive, every crate actually
  compiled into the product, not just the ones explicitly listed in
  the four `Cargo.toml` manifests.
- Covers RustSec's three advisory categories: known
  vulnerabilities, unmaintained crates, and yanked crate versions —
  all three would have produced a non-zero exit and a printed table;
  none did.

## 4. What this does not cover (named, not hidden)

- **Licenses** — `cargo-audit` does not check license compliance;
  that is `cargo-deny`'s `licenses` check specifically, not run this
  pass (no license-policy requirement exists for this project today,
  so none was invented).
- This is a point-in-time scan against the advisory database as
  fetched at run time — it does not itself set up continuous/CI
  scanning (no such CI pipeline exists in this repository to wire it
  into).

## 5. Verdict

**DEPENDENCY SECURITY = PASS** — real `cargo-audit` scan, real
RustSec advisory database (1,277 entries), all 252 resolved workspace
dependencies (direct + transitive), zero vulnerabilities, zero
unmaintained-crate warnings, zero yanked-version warnings. This
directly closes Increment 13's own named gap ("a formal dependency-
advisory scan... was not run").
