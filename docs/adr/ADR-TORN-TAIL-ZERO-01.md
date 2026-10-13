# ADR-TORN-TAIL-ZERO-01 — A zero-filled tail is a torn tail, in the WAL and in the MANIFEST

| | |
|---|---|
| Status | **ACCEPTED and IMPLEMENTED 2026-10-13** — maintainer approval 2026-10-13 ("Approve all recommended fixes, then proceed to D1b"): a region that is entirely zero from a frame boundary to end-of-file is a torn tail, in the WAL and in the MANIFEST. The proposal text below is kept as written; the implementation record is the last section. |
| Date | 2026-10-13 |
| Findings | **F-D0-1** (WAL), **F-D0-2** (MANIFEST) — docs/PERSISTENCE_MAP.md §9, docs/D0_BASELINE.md probes H1/H2 |
| Touches (if fix) | `src/wal/recovery.rs` (+18 lines), `src/manifest/recovery.rs` (+23 lines) — **both protected** — and **one assertion in an existing test** (§5) |
| Prototype | `docs/adr/prototypes/D1a-gate-fixes.diff` (hunks `src/wal/recovery.rs`, `src/manifest/recovery.rs`) |
| Tests | `tests/pending_fixes.rs`: `f_d0_1_*` (2), `f_d0_2_*` (1) red today / green on the prototype; `guard_f_d0_1_*` (2), `guard_f_d0_2_*` (1) green on both |

Tags: `[R]` read, `[M]` measured, `[P]` prototype, `[A]` analysis.

## 1. Context

A file whose **length** was extended but whose **data** never reached the disk reads back as zeros. That is the classic crash artefact of delayed allocation (`docs/DURABILITY_CONTRACT.md` A3 / model M5). Whether *this* NTFS produces it after a real power cut is **NOT TESTED**; D1b's simulator will produce it by construction.

Current behaviour `[M]` (D0 probes; re-run now as `tests/pending_fixes.rs`):

| shape | WAL (3 acknowledged records, then the tail) | MANIFEST (2 edits, then the tail) |
|---|---|---|
| 4 or 8 zero bytes | torn: 3 trusted | 4: torn; **8+: open fails** `Corruption … empty body (missing edit_type)` |
| 16, 64, 4096 zero bytes | **whole segment corrupt: 0 of 3 trusted**, `corrupted_segments=[1]` | **open fails** |
| a valid frame then 4096 zeros | **0 of 4 trusted** | — |

Cause `[R]`: an all-zero 8-byte frame header is `length=0, crc=0`, and `crc32c("") == 0`, so the CRC check *passes*; the empty body then fails to decode; the frame is not provably the last by the extent rule, so it is classified corruption (WAL, `wal/recovery.rs:125-186`) or returned as an error regardless of position (MANIFEST, `manifest/recovery.rs:124-129`).

Consequences today: **product** (`wal_preflight`): refuses to start with `WAL_CORRUPT` — fail-closed, nothing destroyed, but a recoverable database is unusable until an operator intervenes; **bare engine** (`LsmEngine::open`, contract gap G7): silently discards the whole segment including its acknowledged records.

## 2. Decision requested and options

* **Fix** — classify as torn any tail that is **entirely zero from a frame boundary to the end of the file**, of any length. *Recommended.*
* **Accept with contract text** — keep the behaviour; state that a zero-filled tail of 9+ bytes is "corruption, operator action required".
* **Exclude** — declare the shape unreachable on supported platforms. Not supportable without Tier 3/4 evidence.

## 3. The rule and why it is safe

> **If the 8-byte frame header at offset `o` is all zero and every byte from `o` to the end of the file is zero, the file ends at `o`: torn tail, truncated, reported.**

*Safe because* no real frame is all zero: a WAL body is at least `seq(8) + op(1)` bytes and a MANIFEST body at least the edit-type byte, so a genuine frame always has a non-zero length or CRC field. A zero region therefore cannot hide an acknowledged record. *Narrow because* the zero run must reach end-of-file: a zeroed header with valid frames after it, or any non-zero byte after the zeros, still fails the old way (corruption) — pinned by `guard_f_d0_1_damage_followed_by_valid_frames_is_still_corruption`, `guard_f_d0_2_corrupt_middle_frame_is_still_an_error`, `guard_f_d0_1_all_ff_tail_never_yields_extra_records` `[M]` green on both trees.

**What the fix does not claim.** The existing contract clause C-TORN-1(b) says a torn tail may be "short, partial, zero-filled, **stale garbage**, 0xFF". The fix covers zero-filled only. A garbage (non-zero) tail whose claimed extent does not end exactly at EOF remains corruption (it cannot be told from mid-file damage without scanning ahead). **Clause (b) is therefore narrowed in the proposed text** (§6) — this is a change to a stated contract and the maintainer should see it as one.

Cost `[A]`: one 8-byte compare per frame; the zero scan runs only when a header is all zero (reads the tail once, 4 KiB at a time, bounded memory). MANIFEST: same.

## 4. Prototype results `[P]`

`f_d0_1_zero_filled_wal_tail_of_any_length_is_a_torn_tail` (4, 8, 16, 64, 4096 bytes), `f_d0_1_valid_unacknowledged_frame_then_zeros_keeps_the_valid_frame`, `f_d0_2_zero_filled_manifest_tail_of_any_length_is_a_torn_tail` (4, 8, 16, 64; open then truncates back to the previous length and keeps both edits): **red on the current tree, green on the prototype**; the three guards green on both. The whole `fault_points` suite passes on the prototype.

## 5. A conflict with existing, deliberately-pinned tests — needs the maintainer's explicit authorisation

`src/ops/wal_tail_tests.rs::existing_corruption_refusals_take_precedence_and_are_unchanged` (lines 687–705) asserts, for a directory with a **64-byte zero tail appended**, that the startup guard returns `codes::WAL_CORRUPT` for both override settings. **That test fails on the prototype** (`unwrap_err()` on `Ok`: the guard now quarantines the 64 bytes and starts). A second existing test pins the same shape: `cli/tests/f07_tail_damage_integration.rs::existing_wal_corrupt_refusals_are_unchanged_and_the_override_never_bypasses_them` lists `Mutation::Z3Zero64` among the "corrupt, refused, unchanged" mutations and fails on the prototype at the "a corrupt WAL must stay refused" panic. **These two are the only existing tests that fail anywhere in the workspace on the prototype** `[P]` (55 binaries; the other 53 results are identical to the clean run). Attribution is exact: with only the `src/wal/recovery.rs` hunk reverted in the prototype, both pass (`[M]`, run separately).

Why it exists: F-07 (ADR-WAL-01, `PHASE_ITEM_F07_ADR.md`) deliberately left `src/wal` classification untouched ("Any change under `src/wal/` (including P1's classification)" is listed as out of scope) and pinned the shape as *unchanged*; `PHASE_ITEM_F07_DISCOVERY.md` item 2 recorded it as an open observation: *"A zero-filled tail of 9 or more bytes is refused as WAL_CORRUPT although no acknowledged data is damaged (Z3). Reachability from a real crash not tested."* So the test pins a known, documented limitation, not a requirement.

Required edit if the fix is authorised, **to those two assertions only** — in the library test the last loop (the `zero` directory), in the CLI test the `Z3Zero64` entry (to be moved to the existing torn-tail-quarantine tests of the same file, which already cover the other torn shapes): the observed outcome on the prototype `[P]` is `Ok(StartupGuard)` for the first override value — the 64 zero bytes are quarantined (`wal-quarantine/wal-1.188.<ms>.tail`, `bytes: 64`), the attestation is consumed (`attestation_removed: true`), nothing acknowledged is missing and the start proceeds; the assertion would become "the guard succeeds and quarantines exactly the 64-byte tail" (and the attestation/no-attestation variants of the torn-tail policy tests that already exist for other torn shapes apply unchanged). The first two blocks of the test (middle-frame damage, header damage) are untouched and still pass. This is a change of an asserted outcome of a certified refusal; I have **not** made it. Per CLAUDE.md ("Fix the test only if the test is proven wrong. Document why.") the justification is: the test was written to prove F-07 did not alter the classifier, F-07's own ADR deferred the classification to a later authorised change, and this ADR is that change.

## 6. Contract text (PROPOSED; drafted in `docs/DURABILITY_CONTRACT.md` §12)

**C-TORN-1(b), narrowed.** *Recovery treats as a torn tail, truncates and reports: (b1) an incomplete frame; (b2) a frame whose claimed extent ends at the end of the file and fails its check; (b3) **a region that is entirely zero from a frame boundary to the end of the file, of any length**. Anything else that fails to validate before a later valid frame, or that is non-zero and not (b1)/(b2), is corruption and is reported, never truncated silently.* Stale-garbage tails are no longer promised to be treated as torn.

**If accepted instead:** *"a zero-filled tail of 9 bytes or more on the WAL (and 8 or more on the MANIFEST) makes the directory unopenable until an operator removes the tail; the product start refuses, the bare engine drops the segment."*

## 7. Scope and line-by-line

`src/wal/recovery.rs`: one block at the top of the frame loop (header all zero → read the rest in 4 KiB chunks → if all zero: `truncated = true; break`; else seek back and continue with the existing code). `src/manifest/recovery.rs`: the same test before the length/CRC checks, plus `rest_is_zero`. No call site, signature or on-disk format changes; no change to what is written. Recovery of a non-zero tail is byte-for-byte the old code.

## 8. Placement

Before D1c (the class-C simulator will emit zero tails natively, so their expected state must be decided first). Not needed for D1b's *infrastructure*.

---

## 8. Implementation record (2026-10-13)

Landed: `src/wal/recovery.rs` (an all-zero 8-byte header whose remainder to end-of-segment is all zero is a torn tail, whatever its length) and `src/manifest/recovery.rs` (`rest_is_zero` and the same rule). The two assertions the F-07 mission pinned were changed **with the maintainer's explicit authorisation recorded above**, from "a 64-byte zero WAL tail is `WAL_CORRUPT`" to "a 64-byte zero WAL tail is a torn tail": `ops::wal_tail_tests::existing_corruption_refusals_take_precedence_and_are_unchanged` and the `Z3Zero64` entry of `cli/tests/f07_tail_damage_integration.rs::existing_wal_corrupt_refusals_are_unchanged_and_the_override_never_bypasses_them`. Every other entry of those tests (damage that is not an all-zero run to end-of-file) is unchanged and still refused. Tests: `f_d0_1_*` (2), `f_d0_2_*` (1) pass; guards `guard_f_d0_1_*` (2), `guard_f_d0_2_*` (1) pass.

**What this does not cover (found by D1b, `OPEN_ITEMS.md` F-D1b-1):** a tail whose *first* sector was lost while later sectors persisted has an all-zero header followed by non-zero bytes. The rule does not apply, the header reads as length 0 / crc 0, the frame fails to decode, and because its claimed extent does not reach end-of-file it is classified as corruption.
