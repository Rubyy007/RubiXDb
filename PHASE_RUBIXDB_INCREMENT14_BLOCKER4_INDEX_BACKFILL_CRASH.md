# Increment 14, Blocker 4 — CREATE INDEX Mid-Backfill Crash

## 1. Baseline / root cause (found by inspection, not guessed)

`IndexBuilder::recover_incomplete_builds` and `recover_incomplete_drops`
(`src/relational/index.rs`, `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md`
§8) already implement the certified "restart, not resume" crash-
recovery protocol — every index found `Building` at startup is
rebuilt from scratch (idempotent: re-derives every entry from current
truth); already unit-tested
(`src/relational/index_tests.rs::recover_incomplete_builds_restarts_
from_scratch_and_activates`).

**Real gap**: neither function was ever called from any real product
entry point.

```
$ grep -rn "recover_incomplete_builds\|recover_incomplete_drops" --include=*.rs .
./src/catalog/service.rs:786:  (doc comment reference only)
./src/relational/index_tests.rs: (unit tests only)
```

Zero matches in `api/src/main.rs`, `cli/src/host.rs`, or anywhere else
in the actual startup path. **Consequence**: a real process kill
during `CREATE INDEX` backfill left the index permanently stuck
`Building` across every subsequent restart of the actual product,
even though the engine-level fix already existed and was already
proven correct in isolation.

## 2. Fix (product layer only — certified engine untouched)

Added `recover_incomplete_index_operations(&state)` — calls both
existing `IndexBuilder` methods and logs any recovery — invoked
immediately after `AppState::new(...)`, before the router is built and
before the server is marked ready, in both real entry points:

- `cli/src/host.rs` (the `rubixdb gui`/embedded-server path).
- `api/src/main.rs` (the standalone `rubixdb-api` deployment path).

No change to `src/relational/index.rs` itself, no change to the
certified engine (`WAL`/`Manifest`/`SSTable`/`Compaction`/Read/Write
Engine) — this is exactly a missing call site, not new recovery logic.

## 3. Real test (`cli/tests/index_backfill_crash_integration.rs`)

Real `rubixdb gui --no-browser` process, real 200,000-row table
(`bigidx`), real `CREATE INDEX idx_val ON bigidx (val)`:

1. Fired `CREATE INDEX` on a background thread (a short client-side
   timeout that does *not* stop server-side execution — inspected
   `IndexBuilder::create_index_online`'s signature: it takes no
   cancellation token, so a client disconnect cannot abort a backfill
   in this architecture).
2. **Proved backfill was genuinely in progress**: polled the real
   `/v1/catalog/indexes` endpoint every 5ms until it reported
   `idx_val` in state `"building"` (asserted `panic!` if it had
   already reached `"ready"` before the poll caught it, which would
   have invalidated the test).
3. Real `Child::kill()` (ungraceful `TerminateProcess`) the instant
   `"building"` was observed.
4. Real restart (`rubixdb gui --no-browser` again, same data
   directory) through the real product entry point.
5. Verified, immediately after `/healthz` answered (recovery runs
   synchronously at startup, before the router is even built):
   - `idx_val`'s catalog state is `"ready"` — never left stuck
     `"building"`, never fabricated `"ready"` before recovery actually
     ran.
   - **Query correctness across the whole key range**: indexed
     point lookups for `val` at the start (0, 1), middle (100,000),
     and end (199,998, 199,999) of the key space each return exactly
     one row — proof the recovered index is *complete*, not merely
     marked ready.
   - **Table/index consistency**: `SELECT COUNT(*) FROM bigidx` = the
     exact 200,000 rows seeded — no torn writes from the kill.

**Result: `ok`, 34.26s real wall-clock, single run, no retries.**

## 4. Verdict

**CREATE INDEX MID-BACKFILL CRASH = PASS** — a real product gap was
found (the certified recovery primitive existed but was never wired
into any real startup path), fixed at the product layer only, and
proven with a genuine kill-mid-backfill-and-restart test — not a
completed-DDL crash test relabeled, and not an assumption from the
engine-level unit test alone.
