# ADR-WAL-01 — A damaged WAL tail must not open silently: attested clean stop, loud and non-destructive truncation

**Status: PROPOSED — awaiting maintainer approval. Do not apply.** Nothing in this ADR has been implemented; no engine, protected-path, test, `Cargo.toml` or `Cargo.lock` change was made in the mission that wrote it. Evidence: `PHASE_ITEM_F07_DISCOVERY.md` (2026-10-08, tree `caf12be`). Related and unchanged: ADR-ENG-OPS-001 Finding A (`PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md:107-115`, the engine ignores `corrupted_segments`), which this ADR does not decide.

## Decision

At every product start, refuse to open (unless the operator opts in) when the WAL no longer reaches the sequence number that the previous **graceful** shutdown recorded in a new product-layer attestation file, and in every other case where recovery will truncate a damaged tail, first copy the removed bytes to a quarantine file and report the truncation to the operator — all in the product layer (`src/ops`, the host), with no change under `src/wal/` and no change to the engine's recovery classification.

## Reason (measured, `PHASE_ITEM_F07_DISCOVERY.md`)

* A bit flip in the body (A1), in the CRC field (A2), a raised length (A4) or a genuine 10-byte torn write (A5) of the **last** WAL frame opens, with the acknowledged row 6 gone, after a clean stop **and** after a kill (table 4.2: `rows [1,2,3,4,5]`, `acked rows missing: 6`, 8 of 8 runs). No line is written anywhere (`server.out`, `server.err`, `security.log`, `/readyz`, `/v1/metrics/system`: nothing about the WAL).
* The restart destroys the evidence: WAL 762 -> 705 bytes (`src/wal/mod.rs:578`). `rubixdb check` warned `WAL_TORN_TAIL` (exit 1) before the restart and reports 0 warnings after it.
* A graceful stop and a kill leave the **same** WAL (762 bytes, the same 8 frames): the disk cannot tell them apart, so the engine cannot know whether tail damage "could have been a crash". After a graceful stop it cannot have been one.
* The classifier treats the last frame as torn purely by position (`src/wal/recovery.rs:125-152`) — WAL Spec §6.2 step 5 is explicit about it — so the ambiguity cannot be removed from the WAL bytes; it can only be removed by an extra fact. Damage anywhere else is already refused at the product layer (B1-B3, C1-C3, exit 1, directory unmodified), through `startup_guard` (`src/ops/format.rs:111-135`), which already runs `wal::replay_streaming` and simply never looks at its `truncated` field.

## Alternatives considered (P1-P4; failure mode of each)

| | Rule | Fixes | Failure mode it leaves open or creates | Protected path? |
|---|---|---|---|---|
| **P1** | physically short last frame = torn; complete-length last frame with a bad CRC = corruption, fail closed | A1, A2 | A4 (raised length) still silent loss; **false refusals** after a real power loss if the filesystem leaves a complete-length, partly unwritten tail frame — the case Spec §6.2 step 5 exists for, and power loss is untested | **yes** (`recovery.rs:142-152`, `:195-203`) and contradicts the certified Spec |
| **P2** | log-and-open for corruption | nothing | opens B/C/Z3 with a log line instead of refusing: **regresses** the product-layer fix for ADR-ENG-OPS-001 Finding A | no |
| **P3** | P1 + `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1` | A1, A2 (refuse by default) | P1's, plus a one-flag path to data loss; no help for the ambiguity | yes (as P1) |
| **P4 (this ADR)** | attested clean stop + quarantined, reported truncation, override on the refusal | A1, A2, A4, A5 **after a graceful stop** | after a **kill / power loss** a damaged acknowledged last frame is still truncated (no attestation exists): loss is possible but no longer silent or destructive; standalone `rubixdb-api` writes no attestation | **no** |

P1's attribution to external systems in the mission text was not verified (Discovery §6.1) and plays no part in this decision.

## Design

**Attestation file** `<data_dir>/WAL_CLEAN_STOP`, four `key=value` lines and a trailing `crc32c=` of the preceding text: `rubixdb-wal-clean-stop=1`, `segment=<id>`, `length=<bytes>`, `last_seq=<seq>`.

* **Written** by the host after the engine has fully shut down and released the WAL lock, from a read-only `wal::replay_streaming` of the stopped directory: only if it reports `corrupted_segments_count == 0` and `truncated == false` and the last segment's length equals `last_valid_position.offset`. Written as temp file + fsync + rename. Failure to write it is logged and does not fail the shutdown.
* **Evaluated** in `startup_guard`'s successor, before the engine opens, from the same `replay_streaming` summary the guard already computes plus the manifest checkpoint (`manifest::replay_readonly(dir)?.state.checkpoint_seq()`, a read already performed by `LsmEngine::open`, `src/lsm/mod.rs:1354-1355`). **Refuse iff `max(last valid WAL seq, checkpoint_seq) < attested last_seq`.** Using sequence numbers makes the rule immune to a stale file: the WAL is append-only and a later graceful stop rewrites the file, so a log that has grown or been legitimately purged below the checkpoint (purge only removes segments the manifest already covers) never trips it; only a log that has lost acknowledged tail records does.
* **Removed** at open, after the guard has passed and before the engine opens (best effort; a stale file is harmless by the rule above).
* **Unattested cases** (kill, power loss, first start, legacy directory, standalone `rubixdb-api`, a missing/unreadable/unknown-version file): no refusal; if `truncated` is true the guard proceeds to quarantine, below.

**Quarantine.** When the summary reports `truncated == true`, before the engine's `set_len`, copy the bytes `[last_valid_position.offset, file length)` of segment `last_valid_position.segment_id` to `<data_dir>/wal-quarantine/wal-<segment>.<offset>.<unix_ms>.tail` (temp + fsync + rename; a small header with segment id, offset, length and a CRC32C of the bytes, so a partial file is recognisable). Report it in three places: a stderr line at startup; a new security-log event (e.g. `wal.tail_quarantined`) with segment, offset, byte count, last good seq — never the bytes; and a `rubixdb check` finding listing quarantine files. The copy is bounded by the segment size (`DEFAULT_MAX_SEGMENT_SIZE`, `src/wal/format.rs:51`). If the copy cannot be written (I/O error, disk full), refuse to open unless the override is set: the evidence is not destroyed silently.

**Override.** `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1` lets a start proceed **only past the attestation refusal**, as the unattested case: quarantine, report, truncate. It never bypasses `WAL_CORRUPT` (B, C, Z3) and is logged (`wal.tail_override`) only when it actually changes an outcome.

## Correctness impact

* **Recovery, durability, the ack protocol: unchanged.** The engine's classification (`recovery.rs`), the Spec, fsync ordering and what an acknowledgement means are not touched; no record that is recovered today stops being recovered, and no record that is dropped today is recovered.
* **What a consumer of the product observes.**
  1. After a graceful stop, if the WAL has lost any acknowledged tail record (A1, A2, A4, A5 scenarios), the start now **fails** (exit 1, a `WAL_TAIL_DAMAGED`-class refusal naming the segment and the sequence numbers, "the directory has not been modified") instead of opening with fewer rows. The override restores today's outcome.
  2. After a kill, power loss, or any unattested stop, a truncated tail still opens as today, but now with a stderr line, a security-log event, and the removed bytes preserved. `rubixdb check` keeps reporting `WAL_TORN_TAIL` as a warning and adds the quarantine listing.
  3. Everything currently refused stays refused with the same message.
* **Not fixed:** an acknowledged record that is damaged and then followed by a kill or power loss before the next graceful stop is still dropped at the next start. Quarantine makes that visible and potentially recoverable; it does not prevent it.

## Performance impact (predicted only; to be measured in the implementation mission)

* Hot append path and read path: none (nothing runs there).
* Recovery path: one small file read, one manifest checkpoint read, and — only when a tail is truncated — one bounded file write. The WAL walk itself already happens in `startup_guard`.
* Shutdown: one read-only streaming walk of the retained WAL, the same cost the start-up guard already pays (ADR-ENG-OPS-001 §1), plus a small write; this lengthens graceful stop in proportion to the retained WAL size.

## Failure semantics

* **Crash during recovery (before the engine opens):** the guard writes nothing except the quarantine file (temp + rename). A crash leaves either no file, a temp file (ignored), or a complete file; the next start sees the same untruncated tail and finds the existing quarantine entry for the same segment/offset/CRC (skip), or writes it.
* **Crash between quarantine and the engine's truncate / partial truncate:** `set_len` + `sync_all` is the existing step (`mod.rs:578-579`); the file is either old-length or new-length; the quarantine already holds everything that would be removed.
* **Corrupt segment header, or any `corrupted_segments`:** unchanged — `WAL_CORRUPT` refusal; the attestation is not consulted and the override does not apply.
* **Operator override:** see Design; it converts the attestation refusal into the unattested path only.
* **Attestation write fails, is damaged, or is for an unknown version:** ignored with a stderr note; the unattested path applies. Never a refusal on its own.
* **Standalone `rubixdb-api` binary** (unsupported for v1, D-2): shares the guard (`api/src/main.rs:64`) and so gains quarantine and reporting, but writes no attestation.

## Configuration surface

| Item | Value |
|---|---|
| `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL` | unset or empty = off (default); exactly `1` = on; any other value stops startup naming the variable (same strict parsing as the `RUBIXDB_LOCAL_*` family, `cli/src/startup_env.rs`). The name is the one given in the mission; whether it should join the `RUBIXDB_LOCAL_*` family is a maintainer choice. |
| Files | `<data_dir>/WAL_CLEAN_STOP`; `<data_dir>/wal-quarantine/` (never deleted automatically) |
| Refusal text (proposed) | `engine open refused: WAL_TAIL_DAMAGED: the log ends at sequence {Q}, but the last clean shutdown recorded {S} (segment {id}, {len} bytes); {S-Q} acknowledged record(s) are missing from the end of the WAL. Opening would discard them permanently. Run \`rubixdb check\`, restore a verified backup into a new instance, or, if you accept losing them, start once with RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1. The directory has not been modified.` |
| New dependency / `unsafe` | none |

## Migration

* **No on-disk WAL, manifest or SSTable format changes**, so no existing WAL becomes unreadable. Existing directories have no attestation and behave as the unattested case (plus the new reporting).
* **Additive entries in the data directory:** `rubixdb check` flags unknown entries as `Info UNEXPECTED_FILE` (`src/ops/check.rs:809-826`; Info does not change the exit code) — the two new names must be added to its known list. Backup/restore do not enumerate the directory (restore needs an empty destination, `src/ops/restore.rs:141`), so the attestation and quarantine are not part of a backup — to be confirmed by the implementation's tests.
* **Existing tests:** none identified that must change. The engine-level suites do not pass through `startup_guard` (callers are only `cli/src/host.rs:209`, `api/src/main.rs:64`, `src/ops/open.rs:41`): `tests/wal_tests.rs`, `tests/pathological_recovery_matrix.rs`, `tests/crash_consistency.rs`, `tests/group_commit/crash_consistency.rs`, the `src/wal/fuzz_tests.rs` property tests, and the crash-cycle examples are expected to be unaffected, and none of the four crash-consistency suites is expected to fail. `src/ops/physical_tests.rs::a_torn_wal_tail_is_a_warning_and_a_mid_segment_bad_frame_is_corruption` exercises `check_physical`, not the guard. Product tests that stop gracefully and then damage the tail and expect an open (none found by search) would change. This is a prediction from reading and searching, to be confirmed by running the suites.
* **Rollback:** deleting the two new entries restores today's behaviour.

## Out of scope

Any change under `src/wal/` (including P1's classification); the engine-level refusal of ADR-ENG-OPS-001 Finding A; making the engine write a seal frame at shutdown (would make the attestation unnecessary but needs an authorised engine change); a durable acknowledged-watermark; power-loss testing; automatic repair of a quarantined tail; exposing quarantine state through `/readyz` or the observability schema; the two separate findings recorded in `OPEN_ITEMS.md` (`rubixdb check` modifies the directory; zero-filled tail refused); F-08, F-11, F-18 and everything else.
