# ADR-TORN-TAIL-LOST-SECTOR-01 — A tail record whose first sector was lost is a torn tail, in the WAL and in the MANIFEST

| | |
|---|---|
| Status | **IMPLEMENTED 2026-10-14** (PROPOSED 2026-10-14, decided FIX by the maintainer in the D1b/D1c STEP 1 instruction). Implementation record: §8. **Residual shape not covered: §7 (F-D1b-3 / G15), awaiting a maintainer decision.** |
| Date | 2026-10-14 |
| Finding | **F-D1b-1** (`OPEN_ITEMS.md`; gap **G14**, `docs/DURABILITY_CONTRACT.md` §11; `docs/PERSISTENCE_MODEL.md` §7.1) |
| Extends | `docs/adr/ADR-TORN-TAIL-ZERO-01.md` (a tail that is zero to end-of-file) |
| Touches | `src/wal/recovery.rs`, `src/manifest/recovery.rs` — **both protected**; this ADR is the required record. No on-disk format, signature or call-site change. |
| Tests | `tests/pending_fixes.rs`: `f_d1b_1_*` (2) red before / green after; `guard_f_d1b_1_*` (4) green before and after. Unit tests in `src/wal/recovery.rs` (4, including every in-sector alignment of the record start, 1,100 offsets). P4 over 400 seeds. |

Tags: `[R]` read, `[M]` measured, `[A]` analysis.

## 1. Context `[M]`

D1b's simulator (`docs/PERSISTENCE_MODEL.md` M3/M5) persists a record sector by sector. When the **first** sector of the last record is lost and later sectors persist, the tail starts with a zero run and continues with non-zero bytes. P4 hit this in **108 of 400 seeds**; the smoke cycle hit it in 5–6 rows per run.

Current behaviour `[R]`: the frame header at the record start reads `length = 0, crc = 0`. `crc32c("") == 0`, so the CRC check passes; the empty body then fails to decode; because the (zero) claimed extent `offset + 8` is not end-of-file, the failure is **not** the tail by the extent rule and is classified **corruption** (WAL: `corrupted_segments=[n]`, the product start refuses with `WAL_CORRUPT`; MANIFEST: `replay` returns `Corruption … empty body (missing edit_type)` and the engine does not open). Nothing acknowledged is damaged — the lost record was never synced — but a recoverable database is unusable until an operator intervenes. ADR-TORN-TAIL-ZERO-01 covers only a zero run that reaches end-of-file.

## 2. Decision (maintainer)

**FIX.** When recovery meets, at a frame boundary, a frame whose first sector is all zero and whose remainder (to end-of-file) is non-zero, and the shape is the tail of a single record, classify it **torn tail**: truncate at the frame boundary and report it. Same for the MANIFEST.

## 3. The rule

At offset `o` (a frame boundary) where a frame has failed validation as something other than the provable tail — i.e. at the point where recovery would otherwise declare **corruption** (length out of range, CRC mismatch, or a body that does not decode; an all-zero tail to end-of-file was already decided by ADR-TORN-TAIL-ZERO-01) — the tail is a **lost-first-sector tail**, torn and truncated at `o`, iff **all** hold:

1. **First sector zero.** Every byte of `[o, z)` is zero, where `z` is the next 512-byte boundary after `o`, and `z < end-of-file`. (512 is the smallest sector the model and real devices use; a lost 4096-byte sector satisfies this a fortiori. A header that straddles the boundary is therefore only partly zero — an exhaustive unit test found this case; the first version of the rule required an all-zero 8-byte header and missed it when the record began within 7 bytes of a boundary.)
2. **One frame's worth.** `end-of-file − o ≤ 8 + max_record_len` (WAL: `max_record_len`, 64 MiB by default; MANIFEST: 64). A longer tail cannot be one record and keeps the old classification.
3. **No valid frame behind the hole.** No offset `p ≥ z` in the tail starts a complete frame that is valid: `0 < length ≤ max`, `p + 8 + length ≤ end-of-file`, CRC matches, body decodes. If one exists, the zero run sits in front of data that recovery can read — mid-file damage — and stays **corruption** (the guard of ADR-TORN-TAIL-ZERO-01, extended). The scan is byte-granular, bounded by a CRC work budget (64 MiB of CRC input per tail); exceeding the budget **fails closed** (corruption).

Anything else keeps the old code path byte for byte.

*Why it is safe `[A]`.* A real frame is never all-zero in its header (body ≥ 9 bytes WAL / ≥ 1 MANIFEST, so length or CRC is non-zero), so the zero run is a hole, not a record. Rule 3 means the rule can never discard a readable acknowledged frame: a hole with a valid frame after it is corruption, as before.

*What it cannot tell apart, stated plainly.* An **acknowledged** last record whose first sector is later destroyed by the medium (not by a crash), with nothing readable after it, is now truncated and reported (`truncated=true`, the existing torn-tail report) instead of refused. It is the same ambiguity the existing leniency has for a last frame with a CRC mismatch, and it is reported, never silent. The contract text (C-TORN-1) is extended to say so.

*Not covered.* Several unsynced frames in one tail where the first lost its first sector and **later frames are complete and valid**: rule 3 keeps this **corruption**. It cannot be told from damage in front of acknowledged data without knowing the last synced offset, which recovery does not have. Any such P4/smoke remainder is reported with this reason.

## 4. Cost `[A]`

Zero for the common path (one 8-byte compare per frame, already present). On the rare lost-sector path: one read of the tail (≤ 8 + max_record_len bytes, once) and a byte-granular header scan; CRC work bounded by §3.3.

## 5. Contract text

`docs/DURABILITY_CONTRACT.md` C-TORN-1(b) gains **(b4)**: *a frame whose header is zero and whose first sector (to the next 512-byte boundary) is entirely zero, with no readable valid frame after it and no more than one maximum-size frame to end-of-file, is a torn tail.* Gap G14 closes.

## 6. Alternatives rejected

* Treat every zero-header frame as torn regardless of what follows — loses acknowledged records behind a mid-file hole (rule 3 exists to prevent this).
* Require the zero run to end exactly on a sector boundary — a persisted sector can legitimately start with zero bytes; the first non-zero byte need not be aligned.
* Accept with contract text — rejected by the maintainer.

## 7. What this does not cover — F-D1b-3 / gap G15 (found by the STEP 1 re-run)

After this change the smoke cycle still produces 3–5 class C rows per run in which recovery rejects the WAL as corrupt (symptom label "F-D1b-1" in `smoke.tsv`). Inspected byte by byte (four of the five rows of one run): the zero run is **exactly half of the write** and ends **mid-sector** (41 of an 83-byte frame; 78 of 156 bytes). That is the model's M3, which tears the final sector of the last persisted write at *half-sector* granularity ("first half, second half, or none"). It contradicts A2 ("a physical sector write is atomic"), so it is not a lost-first-sector shape. Two sub-shapes occur: a single frame with an unaligned zero prefix, and a group-commit batch in which the first frame is lost while a later frame survives intact (rule 3 keeps this one corruption — correctly: it cannot be told from mid-file damage without the last synced offset, which recovery does not have).

*Experiment, not landed:* with M3 conformed to A2 (a sector is persisted whole or not at all) the smoke cycle shows **0** such rows in three runs and the three runs agree (112 / 112 / 112 PASS); P4 shows 0 rejected as corrupt. *Options for the maintainer:* (1) conform M3 to A2 (a model correction); (2) keep M3 as a deliberate stress beyond A2 and widen the rule to any zero prefix of the tail frame — which also turns the guard `guard_f_d1b_1_only_the_header_zeroed_is_still_corruption` into a torn tail, a wider change that this ADR does not make — and/or accept the batch sub-shape as fail-closed corruption with contract text. No change was made to the model or to the rule beyond the decision above.

## 8. Implementation record (2026-10-14)

Landed in `src/wal/recovery.rs` (`lost_first_sector_tail`, called through `classify_failure_or_lost_sector` from the three places `walk_segment` would declare corruption) and `src/manifest/recovery.rs` (the same function, called before the three errors `replay` would return). Regression tests as in the header table. Measured: `pending_fixes` `f_d1b_1_*` red before (WAL: `records=0 truncated=false corrupted=[1]`; MANIFEST: `empty body (missing edit_type)`), green after; the four guards green before and after; P4 "rejected as corrupt" 108 → 0 of 400; the other three P4 buckets unchanged (7 / 103 / 290 vs 7 / 103 / 182 + 108). The MANIFEST has one integration test and one guard but no exhaustive-alignment unit test (its frames are at most 72 bytes; the straddling-header path is the same code as the WAL's, which is covered exhaustively). Measured effect on the smoke cycle: `docs/PERSISTENCE_MODEL.md` §7.4.
