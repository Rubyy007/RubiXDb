# ADR-F11-OI03 — stamp a fresh data directory before the engine creates anything (OI-F11-03)

**Date:** 2026-10-10. **Status:** IMPLEMENTED by the F-11 real-closure mission (product layer; no protected path). Format-behaviour change, hence this record. Maintainer review welcome; nothing here weakens a refusal.

## 1. Context (source citations at HEAD)

* `src/ops/format.rs` `ensure_compatible`: marker present and `== CURRENT_DATA_FORMAT (1)` -> `Marked`; another value or an unreadable marker -> refused (`UNSUPPORTED_DATA_FORMAT`); directory absent, or containing only the restore marker -> `Fresh`; **any other content without a marker -> `Legacy`** ("a pre-existing directory with no marker is a *legacy* v1 directory ... accepted and - deliberately - not modified", module doc lines 20-22; test `legacy_directory_is_accepted_and_never_modified`).
* The marker is **declarative, not inferred and not a migration signal today**: the only readers are `ensure_compatible` (via `startup_guard`, `startup_guard_with_tail_policy`, `ops/open.rs`), and what they do with it is compare to 1 and refuse anything else. There is no migration code; `grep -rn DATA_FORMAT src cli api sql instance` finds no other reader (`ops/check.rs:820` only lists it as a known file name). Legacy is *defined* as v1 (the text of the module doc), because every file inside is individually versioned (`RBXWALv1` + version, SSTable footer version, `ROW_FORMAT_VERSION`).
* Risk the characterisation does carry: a FUTURE v2 build that reads the marker to choose a migration branch would take an unstamped directory down the v1 ("legacy") branch. For an unstamped directory that this build created, v1 is the truth, but only because the marker is absent *by an interruption*, not by provenance.
* The defect: the product start stamped **after** `LsmEngine::open` (`cli/src/host.rs`), so a kill in between left data on disk without a marker, and `DATA_FORMAT.tmp` (written by `stamp_if_fresh`, which did not `fsync` it) made the directory non-empty and `Legacy`: never stamped again. Measured 3 of 3 (`PHASE_ITEM_F11_CLOSURE.md` 10.2).

## 2. Fix branches considered

1. **Auto-stamp a `Legacy` directory after a successful open** (the expected branch). Rejected: it changes the documented, tested rule "legacy is never modified" for directories from earlier releases, and it *guesses* a version for data this build did not create.
2. **Stamp before the engine creates anything** (chosen). Removes the window instead of repairing it: after the guard decides `Fresh` and before `LsmEngine::open`, `stamp_if_fresh` writes the marker (temp file, `fsync`, atomic rename; creates the directory if absent; never overwrites a marker; no-op unless `Fresh`).
3. **Recognise this build's own interrupted stamp** (chosen, for directories already in the field state and for kills inside the stamp). `ensure_compatible` ignores `DATA_FORMAT.tmp` when deciding "is the directory empty" and, when data exists next to a `DATA_FORMAT.tmp` whose text is **byte-for-byte the text this build writes**, classifies the directory `Fresh` so the next start completes the stamp. The file is proof of provenance; a temp file with any other text proves nothing and the directory stays `Legacy`, unmodified.
4. Leave as is, document. Rejected: it keeps a state that the product itself creates and no start ever leaves.

## 3. Decision

Branches 2 and 3. `cli/src/host.rs` stamps before the engine open (the old after-open call is removed); `stamp_if_fresh` gains `fsync` of the temp file and directory creation; `ensure_compatible` gains the interrupted-stamp rule. `ops/open.rs` (check / restore) and `api/src/main.rs` (standalone, unsupported) still stamp after their open and now also benefit from rule 3 at their next start. A directory from an earlier release (no `DATA_FORMAT`, no valid `DATA_FORMAT.tmp`) is still `Legacy` and untouched.

Consequence accepted: a first start whose open then fails leaves an empty directory holding only `DATA_FORMAT` (it was empty before): classified `Marked`, which is correct.

## 4. Evidence

* Unit (`src/ops/format.rs`): `a_lone_interrupted_stamp_is_still_a_fresh_directory_and_gets_stamped`, `data_beside_this_builds_own_stamp_temp_is_stamped_not_left_legacy`, `data_beside_a_temp_file_this_build_did_not_write_stays_legacy_and_unmodified`, `stamping_creates_a_missing_directory_and_never_overwrites_a_marker`, the existing legacy / garbled-marker tests unchanged.
* Real binary (`cli/tests/f11_startup_interruption_integration.rs`, `--features startup-probe`): `interrupted_between_the_format_marker_temp_and_its_rename` (parked after the temp write: no engine file exists yet; after the hard exit and a restart `DATA_FORMAT` is present, no temp; `restart_and_verify_fresh` now asserts the stamp for every fresh restart, including the 40 empty-directory kills of the sweep), `data_left_beside_this_builds_stamp_temp_is_stamped_at_the_next_start` (the old-order state), `data_beside_a_foreign_temp_file_is_not_stamped_and_nothing_is_lost` (control).
* Non-vacuity: the same tests against the unchanged `92f8774` code fail (`E:\rubixdb_f11c\logs\fix_negative_old_code.log`: stamp absent after the restart; the engine's WAL exists at the pause).

## 5. Criteria and status

| | Criterion | Status |
|---|---|---|
| C1 | no instant at which engine data exists without a marker, for a directory this build creates | PASS (pause test: no WAL file at the temp-written point) |
| C2 | a directory from an earlier release is never stamped or modified | PASS (unit + real-binary control) |
| C3 | an interrupted stamp of this build is completed at the next start, nothing lost | PASS (unit + real binary) |
| C4 | an unsupported / garbled marker is still refused unchanged | PASS (existing test) |
| C5 | a future migration reading the marker would find one on every directory this build creates | PASS for this build's directories; directories created before this change by an interrupted start without a temp file remain indistinguishable from legacy (NOT FIXED, unknowable) |
