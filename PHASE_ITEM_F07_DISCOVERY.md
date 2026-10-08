# PHASE ITEM F-07 — DISCOVERY: damaged WAL tail opens silently and acknowledged rows disappear

**Date:** 2026-10-08. **Mission:** F-07 only; discovery and ADR, no engine behaviour changed, nothing under `src/wal/` touched. Companion document: `PHASE_ITEM_F07_ADR.md` (ADR-WAL-01, PROPOSED).

## 1. Identity

| Item | Value |
|---|---|
| Repository | branch `master`, `git status --short` empty, HEAD `caf12bef8161ff4ddb35442bd331b9a916ce0138` |
| `git log -3 --oneline` | `caf12be` item C: doc corrections ... / `ac7fa26` item C: certification, closure records ... / `a5a9bbe` item C: operator-settable cap on the blocking thread pool ... |
| Binary | `cargo build --release --locked -p rubixdb-cli` -> `E:\rubixdb_f07\rubixdb_f07_base.exe`, SHA-256 `8299dffa2c5892b6c0ac5743771b4ade92335f16fb4b4ced00776b9ef7f98d74` |
| Reported identity | `GET /v1/observability/version` -> `git_revision: caf12bef8161`, `build_identifier: 0.1.0-release-x86_64-windows` (raw: `raw\identity_version.json`) |
| Host | Windows 10 Home, release build, loopback, local disk; one instance at a time on port 302 |
| Engine-layer probe | an unmodified `git archive HEAD` copy at `E:\rubixdb_f07\tree` plus one added example (`examples\f07_engine_open.rs`), built to `E:\rubixdb_f07\tree_target`; the repository itself was not built into or modified |

## 2. What F-07 says, and where it comes from

* `OPEN_ITEMS.md` 2026-10-05: "F-07 (WAL final-record damage opens silently; engine ADR territory)".
* `PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_BASELINE.md` V-31 (line 171): last 37 bytes removed, killed instance -> opens, the only INSERT (50 rows) lived in the final record and is gone, no warning. V-32 (line 172): "16 bytes flipped in the middle of the final segment" -> same.
* `PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_CERTIFICATION.md` line 64: unchanged after the lifecycle work; "header damage is refused"; "OPEN (engine ADR territory; protected)".
* Mission statement under test: "damaged WAL tail data can open silently and acknowledged rows can disappear."

## 3. The code paths that decide "open" versus "refuse"

All line numbers are for the tree at `caf12be`. Nothing here was modified.

| # | Decision | Where | What it does |
|---|---|---|---|
| 1 | Segment header valid? | `src/wal/format.rs:87-126`, called from `src/wal/recovery.rs:209-230` | Short file, bad magic, `format_version != 1`, non-zero flags, or id != filename id -> `Corruption`; "never a torn write" (doc `format.rs:82-86`). |
| 2 | Frame header incomplete | `src/wal/recovery.rs:76-79` | fewer than 8 bytes left -> `truncated = true` (torn). |
| 3 | Declared length over `max_record_len` | `src/wal/recovery.rs:100-120` | torn only if the 8-byte header is exactly the last 8 bytes of the file (`header_is_tail`, :110); otherwise corruption. |
| 4 | Body shorter than declared length | `src/wal/recovery.rs:130-136` | always torn: "physically cannot have anything after it". |
| 5 | **CRC mismatch** | `src/wal/recovery.rs:142-152` with `is_tail_frame` (:125-128) and `classify_failure` (:195-203) | **tail iff the frame's *claimed* extent `offset + 8 + length` equals the segment length**; tail -> `truncated`, anything else -> `corrupted`. The CRC mismatch itself carries no information beyond that position test. |
| 6 | Decode failure of a CRC-valid body | `src/wal/recovery.rs:174-187` | same tail test as 5. |
| 7 | Which segment may be torn | `src/wal/mod.rs:543-555` (`scan_directory`), `:1248-1252` (`replay_streaming`) | a torn shape in a non-last segment, or any `corrupted`, -> `corrupted_segments` gets that id and the scan **stops**; that segment's own records are discarded. |
| 8 | **Torn tail on the last segment** | `src/wal/mod.rs:574-590` | `result.truncated = true`; with `mutate` (open_for_recovery) `file.set_len(valid_end_offset)` then `sync_all()` — **the damaged bytes are physically removed; nothing is logged, copied or counted** (`:578-579`). |
| 9 | Does the engine act on it? | `src/lsm/mod.rs:1361` (`let _summary = wal::replay_streaming(...)`) and `:1377` (`let (file_wal, _replay) = FileWal::open_for_recovery(...)`) | **both results are discarded**: `LsmEngine::open` itself neither refuses on `corrupted_segments` nor reports `truncated`. Known and documented as ADR-ENG-OPS-001 Finding A (`PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md:107-115`). |
| 10 | Product compensation | `src/ops/format.rs:111-135` (`startup_guard`), called from `cli/src/host.rs:209`, `api/src/main.rs:64`, `src/ops/open.rs:41` | read-only `wal::replay_streaming` before the engine opens; refuses with `WAL_CORRUPT` iff `corrupted_segments_count > 0`. **`truncated` is not examined** (no `else` for it). |
| 11 | Operator tooling | `src/ops/check.rs:937-958` | `inspect` -> `WAL_CORRUPT` is an error, `r.truncated` is a **warning** `WAL_TORN_TAIL` ("recovery truncates it (expected after a crash)"). The only place the tail case is surfaced. |
| 12 | The contract being implemented | `RubixDB-WAL-Specification-v1.0.md` §6.2 step 5 (line 213), §6.3 (218), §6.4 (222) | a CRC mismatch on the frame at the physical end "is treated as a torn write (the fsync boundary can leave a partially-flushed page ... even at frame granularity)"; the tail is truncated and `truncated: true` is reported "so tests and operators can observe that this happened even though it's the expected, non-alarming case". |
| 13 | What a process kill can and cannot show | `tests/crash_consistency.rs:11-19` | `std::process::abort()` "is **not** a power-loss simulation"; it "cannot exercise the torn-write recovery path itself". Real torn writes need a power cut (power loss is NOT TESTED in this project). |

Reading of 5 together with 8: **a bit flip anywhere in the last frame that leaves its claimed extent at or beyond end-of-file is, by specification, the same event as a torn write**, and recovery removes it without a trace.

## 4. Reproducers

### 4.1 Method (black box, current tree)

`E:\rubixdb_f07\scripts\f07_repro.py` (library `f07_lib.py`). For each scenario and for each of two stop modes — **clean** (`rubixdb instance stop`, exit 0) and **kill** (process kill, not a power loss):

1. fresh instance; `CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)`; six autocommit `INSERT`s, ids 1..6, **each acknowledged HTTP 200**; stop.
2. The newest (only) WAL segment holds 8 frames (`GROUP` records, seq 1-2 = catalog, seq 3..8 = rows 1..6; 762 bytes). **After a clean stop the WAL is byte-for-byte the same shape as after a kill** (762 bytes, the same 8 frames in both): a graceful shutdown leaves no marker of any kind, so the disk cannot tell the two apart.
3. Exactly one mutation is applied from outside to that segment (table below).
4. `rubixdb check --data-dir` on a **copy**; then restart of the real binary on the damaged directory; `SELECT id FROM t ORDER BY id`; the WAL after the restart; every log the instance wrote (`server.out`, `server.err`, `security.log`), `/readyz` and `/v1/metrics/system`; then a second restart.
5. The same mutated WAL files were replayed against the **bare engine** (`LsmEngine::open`, no product guard) with the production WAL and pool settings.

Raw evidence: `E:\rubixdb_f07\raw\<scenario>__<clean|kill>\result.json` (+ `wal_before.bin`, `wal_mutated.bin`, `wal_after_restart.bin`), `raw\engine_layer\`, `raw\matrix_summary.txt`, `raw\engine_layer_summary.txt`.

### 4.2 Results — product layer (the real `rubixdb gui` binary)

The mission's three experiments are (a) the last record (A1, with A2/A4 as the other ways a last frame can be damaged), (b) a middle record (B1, with B2/B3), (c) the segment header (C1, with C2/C3). A3 (length lowered by one) and A5 (a genuine 10-byte torn write) and Z1-Z3 (zero-filled tails) are boundary cases of the same classifier. S0 is the undamaged control.

| scenario | mutation | stop | opened? | exit | acked rows missing | WAL bytes mutated -> after restart | `rubixdb check` (on a copy) | any trace in server.out/err, security.log |
|---|---|---|---|---|---|---|---|---|
| S0_control | no mutation | clean | yes | - | none | 762 -> 762 | exit 0 | none (start/stop lines only) |
| S0_control | no mutation | kill | yes | - | none | 762 -> 762 | exit 0 | none (start/stop lines only) |
| A1_last_body | 1 bit flipped in the BODY of the last frame | clean | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A1_last_body | 1 bit flipped in the BODY of the last frame | kill | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A2_last_crc | 1 bit flipped in the CRC field of the last frame | clean | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A2_last_crc | 1 bit flipped in the CRC field of the last frame | kill | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A3_last_len_down | length field of the last frame 49 -> 48 | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| A3_last_len_down | length field of the last frame 49 -> 48 | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| A4_last_len_up | length field of the last frame 49 -> 51 | clean | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A4_last_len_up | length field of the last frame 49 -> 51 | kill | yes | - | 6 | 762 -> 705 | exit 1 | none (start/stop lines only) |
| A5_torn_10bytes | last 10 bytes removed | clean | yes | - | 6 | 752 -> 705 | exit 1 | none (start/stop lines only) |
| A5_torn_10bytes | last 10 bytes removed | kill | yes | - | 6 | 752 -> 705 | exit 1 | none (start/stop lines only) |
| Z1_zero_tail_5 | 5 zero bytes appended after the last valid frame | clean | yes | - | none | 767 -> 762 | exit 1 | none (start/stop lines only) |
| Z1_zero_tail_5 | 5 zero bytes appended after the last valid frame | kill | yes | - | none | 767 -> 762 | exit 1 | none (start/stop lines only) |
| Z2_zero_tail_8 | 8 zero bytes appended after the last valid frame | clean | yes | - | none | 770 -> 762 | exit 1 | none (start/stop lines only) |
| Z2_zero_tail_8 | 8 zero bytes appended after the last valid frame | kill | yes | - | none | 770 -> 762 | exit 1 | none (start/stop lines only) |
| Z3_zero_tail_64 | 64 zero bytes appended after the last valid frame | clean | NO | 1 | - | 826 -> 826 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| Z3_zero_tail_64 | 64 zero bytes appended after the last valid frame | kill | NO | 1 | - | 826 -> 826 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B1_mid_body | 1 bit flipped in the BODY of a MIDDLE frame | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B1_mid_body | 1 bit flipped in the BODY of a MIDDLE frame | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B2_mid_crc | 1 bit flipped in the CRC field of a MIDDLE frame | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B2_mid_crc | 1 bit flipped in the CRC field of a MIDDLE frame | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B3_catalog_body | 1 bit flipped in the BODY of the second frame | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| B3_catalog_body | 1 bit flipped in the BODY of the second frame | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C1_hdr_magic | 1 bit flipped in the segment header magic | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C1_hdr_magic | 1 bit flipped in the segment header magic | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C2_hdr_version | 1 bit flipped in the segment header format_version | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C2_hdr_version | 1 bit flipped in the segment header format_version | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C3_hdr_segid | 1 bit flipped in the segment header segment_id | clean | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |
| C3_hdr_segid | 1 bit flipped in the segment header segment_id | kill | NO | 1 | - | 762 -> 762 | exit 2 | refusal on stderr: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corr...` |


Mutations: A1 one bit flipped in the **body** of the last frame (seq 8 = row 6); A2 one bit flipped in the **CRC field** of the last frame; A3 last frame's length 49 -> 48; A4 last frame's length 49 -> 51; A5 last 10 bytes removed; Z1/Z2/Z3 5 / 8 / 64 zero bytes appended after the last valid frame; B1 one bit flipped in the body of a **middle** frame (seq 5 = row 3) with three intact frames after it; B2 same, in its CRC field; B3 one bit flipped in the catalog record (seq 2); C1/C2/C3 one bit flipped in the segment header magic / `format_version` / `segment_id`.

Reading the table: "opened?" is whether the server answered `/healthz` within 20 s; "exit" is the process exit code when it did not open; "acked rows missing" lists ids that had been acknowledged with HTTP 200 and are absent after the restart; "WAL bytes" is the newest segment's size just after the mutation and after the restart.

### 4.3 Results — bare engine (`LsmEngine::open`, no `startup_guard`)

Same mutated files. Control = 16 memtable entries.
| scenario | `LsmEngine::open` | memtable entries recovered (control = 16) | WAL files after |
|---|---|---|---|
| S0_control | OK | 16 | `[("wal-00000000000000000001.log", 762)]` |
| A1_last_body | OK | 15 | `[("wal-00000000000000000001.log", 705)]` |
| A2_last_crc | OK | 15 | `[("wal-00000000000000000001.log", 705)]` |
| A3_last_len_down | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| A4_last_len_up | OK | 15 | `[("wal-00000000000000000001.log", 705)]` |
| A5_torn_10bytes | OK | 15 | `[("wal-00000000000000000001.log", 705)]` |
| Z1_zero_tail_5 | OK | 16 | `[("wal-00000000000000000001.log", 762)]` |
| Z2_zero_tail_8 | OK | 16 | `[("wal-00000000000000000001.log", 762)]` |
| Z3_zero_tail_64 | OK | 0 | `[("wal-00000000000000000001.log", 826), ("wal-00000000000000000002.log", 24)]` |
| B1_mid_body | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| B2_mid_crc | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| B3_catalog_body | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| C1_hdr_magic | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| C2_hdr_version | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |
| C3_hdr_segid | OK | 0 | `[("wal-00000000000000000001.log", 762), ("wal-00000000000000000002.log", 24)]` |


### 4.4 Observations that follow from the two tables

1. **Part 1 of F-07 — "damaged WAL tail data can open silently": PROVEN, at both layers, after a clean shutdown and after a kill.** A1 (bit flip in the last frame's body), A2 (bit flip in its CRC field), A4 (its length raised past end-of-file) and A5 (a real 10-byte torn write) all open (`/healthz` 200 in about 0.5 s), `/readyz` answers `{"ready": true, "storage_state": "Healthy", "index_recovery": "complete"}`, and **no line about the WAL appears anywhere**: `server.out` and `security.log` contain only the normal start/stop lines, `server.err` is empty, and `/v1/metrics/system` has no field naming truncation, torn or corrupt data.
2. **Part 2 — "acknowledged rows can disappear": PROVEN.** In A1, A2, A4 and A5, row id 6 (acknowledged with HTTP 200 before the stop) is gone after the restart (`[1,2,3,4,5]`), and stays gone after a second restart. For A1/A2/A4 the damaged frame was an intact, fsynced, acknowledged record whose bytes were altered **after** a graceful shutdown — not a crash artifact in any sense.
3. **The evidence is destroyed, not just ignored.** The restart physically shortens the WAL (762 -> 705 bytes; `src/wal/mod.rs:578`). A `rubixdb check` of the directory before the restart reports `WARNING WAL_TORN_TAIL` (exit 1); the same check after the restart reports 0 warnings (exit 0). The single signal in the product exists only until the first start.
4. **A graceful stop and a kill are indistinguishable on disk** (same 762 bytes, same frames, no marker in either). The engine therefore cannot know whether tail damage "could have been a crash". It also means that after a graceful stop the torn-tail excuse is simply false.
5. **The classifier is position-based, not damage-based, and not symmetric.** Raising the last frame's length (A4) is silent loss; lowering it by one (A3) is a refusal with `WAL_CORRUPT`, because the claimed extent then ends one byte before end-of-file (`recovery.rs:125-128`), although in both cases only the last frame is damaged and nothing valid follows it.
6. **Part (b), a middle record: FAIL-CLOSED at the product layer, SILENT at the engine.** B1, B2 and B3 (and the header cases C1-C3) are refused: `rubixdb gui: failed to start: engine open refused: WAL_CORRUPT: 1 corrupted WAL segment(s) found; refusing to open: ... The directory has not been modified.`, exit 1, WAL bytes unchanged (762 -> 762), `rubixdb check` exit 2. The bare engine, however, opens `OK` with **0** entries recovered (the whole damaged segment is dropped) and creates a fresh segment 2 next to the damaged segment 1 (§4.3) — so the product guard (`startup_guard`) is the only thing standing between those cases and silent loss (ADR-ENG-OPS-001 Finding A, unchanged here). This **refines the baseline wording**: V-32's "bytes flipped in the middle of the final segment" lost the row because the segment held a single record, so the "middle" was inside the final frame; with intact frames after the damaged one the product refuses.
7. **Zero-filled tails.** 5 and 8 zero bytes after the last valid frame (Z1, Z2) open with all six rows and are truncated (767/770 -> 762). **64 zero bytes (Z3) are refused as `WAL_CORRUPT`** although every acknowledged row is intact: a zero 8-byte header decodes as length 0 / CRC 0 (the CRC of an empty body is 0), the empty body fails to decode, and the claimed extent ends 8 bytes in, not at end-of-file, so it is classified as corruption (`recovery.rs:125-128, 174-187`). Whether a real power cut can leave that shape on this host's filesystem was **not tested** (§7). Recorded as a separate item (§8).
8. **`rubixdb check --data-dir` is not read-only.** Run on the original damaged directory (not a copy), it truncates the damaged tail exactly as a restart does (762 -> 705, sha256 changed; `raw\check_truncates_original.txt`) — its logical pass opens the engine through `ops::open` (`src/ops/open.rs:41`). Recorded as a separate item (§8).

## 5. What F-07 turned out to be

F-07 is real, and it is narrower and more specific than "WAL damage opens silently". **Damage confined to the last WAL frame whose claimed extent reaches or passes end-of-file is classified as a torn write by design (`recovery.rs:125-152`, WAL Spec §6.2 step 5), the engine removes it from disk (`mod.rs:578`), `LsmEngine::open` throws the `truncated` signal away (`lsm/mod.rs:1361, 1377`), and the product guard looks only at `corrupted_segments` (`ops/format.rs:116`), so the open is silent, the acknowledged rows in that frame are lost, and the bytes that could prove it are gone.** Damage anywhere else — a middle frame, the segment header, a non-last segment — is refused at the product layer with a clear message, exit 1 and an unmodified directory (the engine alone would still drop it, ADR-ENG-OPS-001). The two things the code does not have are exactly the two things that would remove the ambiguity: **(i) any on-disk record that a shutdown was graceful** — a graceful stop and a kill leave identical WAL bytes — and **(ii) any signal at all when a tail is truncated**.

## 6. Policy comparison

### 6.1 How the current code decides "truncate" versus "fail closed" (summary of §3)

* **Truncate (silently):** header-incomplete tail (`recovery.rs:76-79`), body shorter than declared (`:130-136`), oversize length with exactly the 8-byte header left (`:110`), and a CRC or decode failure whose claimed extent equals the file length (`:125-152, 174-187`) — in the **last** segment only.
* **Fail closed (product layer only):** any other failing frame, any bad header, any torn shape in a non-last segment (`mod.rs:543-555`) -> `WAL_CORRUPT` from `startup_guard` (`ops/format.rs:116-125`). The engine itself does not refuse (`lsm/mod.rs:1361, 1377`).
* So the **current behaviour is not any of P1-P4 as the mission states them**: it is "tail: truncate with no log and no copy; everything else: refuse at the product layer, silently drop at the engine". Of the mission's P2 ("log-and-open"), the "log" half does not exist.

The external attributions in the mission's P1 label ("PostgreSQL pg_checksums / LibreDB RELIABILITY.md model") were **not verified**: no external source was consulted in this mission, `pg_checksums` is to my knowledge a data-page checksum tool and not a WAL tail policy, and I cannot identify the other reference. The policies below are therefore compared on their stated rules and on the measurements above, not on those attributions.

### 6.2 The four policies

| Policy | Rule | A1/A2 (bit flip, last frame, after graceful stop) | A4/A5 (looks physically short) | Z3 / B / C | Needs a change under `src/wal/`? | Failure mode it leaves open |
|---|---|---|---|---|---|---|
| **Current** | tail truncate silently; non-tail refuse (product) | **silent loss** | **silent loss** | refuse | n/a | everything in F-07 |
| **P1** | physically short last frame = torn (truncate); a **complete-length** last frame with a bad CRC = corruption, fail closed | refuse | A4 **silent loss** (a raised length is indistinguishable from a short write); A5 truncate (correct) | unchanged | **yes**: `recovery.rs:142-152` / `:195-203` (protected) and a contradiction of Spec §6.2 step 5 | (i) silent loss for any damage that looks short; (ii) **false refusals** after a real power loss if the filesystem leaves a complete-length but unwritten tail frame — the very case Spec §6.2 step 5 exists for, and power loss is untested; (iii) an engine/spec change that needs an authorised mission |
| **P2** | tail truncate + log-and-open for middle/tail corruption | open + log | open + log | **open + log** | no (policy at the callers) | **worse than today** for B/C/Z3: it reopens ADR-ENG-OPS-001 Finding A at the product layer (silent loss of the whole segment, now with a log line) |
| **P3** | P1 + operator override `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1` | refuse unless overridden | as P1 | as P1 | yes (as P1) | all of P1's, plus an override that makes loss a one-flag operation; the override does nothing for the ambiguity |
| **P4** (recommended) | **attested clean stop**: at graceful shutdown the product records the WAL's end (segment id, length, last seq); at open, if that record exists and the replayed log no longer reaches it, refuse (override available); in **every other case that truncates a tail**, copy the removed bytes to a quarantine file and report loudly **before** the engine truncates | **refuse** | **refuse** (A5 after a graceful stop is a shortened file, not a crash) | unchanged (refuse) | **no**: `src/ops/format.rs`, the host (`cli/src/host.rs`) and a new small module; not a protected path | after a **kill / power loss** (no attestation) a damaged *acknowledged* last frame is still truncated — the loss remains possible, but is no longer silent or destructive (quarantine + WARN + security event); standalone `rubixdb-api` writes no attestation; a lost or damaged attestation degrades to the loud-quarantine case |

### 6.3 Recommendation: P4

**Recommend exactly one: P4 — attested clean stop, plus loud, non-destructive truncation otherwise, with P3's override variable on the refusal branch.**

Reasoning, all from the measurements above:

1. The ambiguity is real and cannot be removed from the WAL bytes (observation 4; Spec §6.2 step 5 is a deliberate rule). Only an extra fact removes it, and the one fact available without touching the engine is "the previous stop was graceful and the log ended *here*". After a graceful stop (the common case, and the case where the torn-write excuse is false) that fact turns A1/A2/A4/A5 into refusals using the same refusal family the product already ships for B/C.
2. It needs **no change under the protected path**: the walk results the rule needs (`last_valid_position`, `truncated`) are already returned by `wal::replay_streaming`, which `startup_guard` already calls (`ops/format.rs:115`). The engine's classification and the Spec stay exactly as certified.
3. In the cases that cannot be disambiguated (kill, power loss, legacy directory) the recommendation stops short of refusing, because the only way to refuse there is P1's false-refusal risk, which cannot be measured on this project (power loss untested). What it can do for those cases is the part that was missing entirely: keep the bytes and say so.
4. It does not weaken any existing refusal (B, C, Z3 stay refused) and does not adopt P2.

What P4 does **not** fix, stated plainly: an acknowledged record damaged on disk and then followed by a kill or power loss before the next graceful stop is still dropped at the next start; the quarantine file lets an operator see and potentially recover it, but the engine will have opened without it.

## 7. Not tested, limits

* **Power loss was not tested** (as everywhere in this project). All "kill" runs are process kills; per `tests/crash_consistency.rs:11-19` those cannot produce a physically torn write. Every statement about what a real power cut leaves behind (including whether a zero-filled or garbage-filled complete-length tail frame can occur on NTFS) is **unverified**.
* One host, one filesystem (NTFS), one table, six single-row autocommit statements in one segment. Multi-segment layouts were exercised only by the existing engine tests (`tests/wal_tests.rs`, `tests/pathological_recovery_matrix.rs`), not by new runs here.
* Only single-bit flips and the listed length/truncation/zero-fill shapes were tried; no random-damage campaign was run in this mission.
* The engine-layer probe is a throw-away example in an unmodified copy of HEAD (outside the repository); it exercises exactly `LsmEngine::open` with `product_wal_config()` / `product_pool_config()`.
* The behaviour of the standalone `rubixdb-api` binary was read from source (`api/src/main.rs:64`, same `startup_guard`), not run (unsupported for v1, D-2).

## 8. Separate findings (recorded in `OPEN_ITEMS.md`, not touched)

1. **`rubixdb check --data-dir` modifies the directory it inspects** (documented "INSPECTION (read only)"): it truncated a damaged WAL tail on the original directory (`raw\check_truncates_original.txt`). Related but distinct from F-07.
2. **A zero-filled tail of 9 or more bytes is refused as `WAL_CORRUPT` although no acknowledged data is damaged** (Z3). Reachability from a real crash not tested.

## 9. Evidence index

`E:\rubixdb_f07\raw\` (30 product runs, `engine_layer\`, `identity_version.json`, `matrix_summary.txt`, `engine_layer_summary.txt`, `check_truncates_original.txt`); `E:\rubixdb_f07\scripts\` (`f07_lib.py`, `f07_repro.py`, `f07_engine_layer.py`, `make_tables.py`, document generators); `E:\rubixdb_f07\tree\examples\f07_engine_open.rs` (the engine probe); `E:\rubixdb_f07\rubixdb_f07_base.exe`.
