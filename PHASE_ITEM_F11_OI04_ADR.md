# ADR-F11-OI04 — the attestation is declined after an aborted recovery flush (OI-F11-04): NOT FIXED, decision needed

**Date:** 2026-10-10. **Status:** PROPOSAL, NOT IMPLEMENTED. The fix that works changes an accepted rule (ADR-WAL-01) and reverses an existing F-07 unit-test assertion; the F-11 mission's hard constraints forbid loosening a test, and the F-07 code is out of this mission's scope. No product code was changed for OI-F11-04.

## 1. What actually happens (reproduced, source-cited)
1. The start replays a WAL whose memtable is over its 4 MiB bound; the flush thread publishes SSTable N, then `pool.rotate()` creates the next WAL segment (header only, 24 bytes) (`src/lsm/mod.rs` flush sequence, steps 3-4).
2. A graceful stop arrives. `LsmEngine::shutdown` shuts the coordinator pool first (`self.pool.shutdown()`), so the flush's `CHECKPOINT_MARKER` submission is rejected: stderr `flush attempt 1 failed (published=Some(...)): aborted: BatchCoordinatorPool is shutting down`. The published table stays for the next start's orphan adoption; data is intact (15 of 15 restarts, exact rows, `rubixdb check` 0).
3. `write_attestation` (`src/ops/wal_tail.rs`) requires `last_valid_position.segment_id == newest segment` and `offset == its length`. The newest segment is the empty rotated one, so it answers `NotEligible("the replay summary does not match the newest segment (last valid position segment N offset M, newest segment N+1 length 24)")`, and the stop prints that on stderr; no attestation.

So the decline is not a timing accident of the stop: it is the **attestation eligibility rule meeting a legitimate state** (a rotated segment with no record yet). By the same rule it would also decline after any rotation not yet followed by a record (not tested separately).

## 2. Why it is not fixed here
* The only deterministic fixes are: (a) **change the eligibility rule** (accept a newest segment that holds only its header when the last valid record ends exactly at the end of the preceding segment); (b) make the stop **complete the flush** instead of aborting it (changes the engine's shutdown ordering and the data outcome: no orphan table; `src/lsm` shutdown, not a protected path, but an engine behaviour the mission says must not change); (c) have the host wait for an abort to finish: does not help, the empty segment exists whether or not the abort has completed.
* (a) is the right one and is small, but `the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory` in `src/ops/wal_tail_tests.rs` asserts the **opposite for exactly this shape** ("the newest segment is a header-only file after earlier ones: the final length check fails"). That assertion was a deliberate part of the approved ADR-WAL-01. Applying (a) means reversing it. This mission may not loosen a test or change an approved ADR's rule on its own authority, and F-07 is explicitly out of scope.

## 3. The proposal, measured but not applied (scratch tree, not in the repository)
Patch (`E:\rubixdb_f11c\proto_patch.py`, applied to a scratch copy only): in `write_attestation`, when the exact test fails but `records_replayed > 0`, the last valid position is the end of its segment, and **every newer segment is exactly 24 bytes**, attest `{segment: lvp.segment_id, length: lvp.offset, last_seq: lvp.seq}`. Nothing else changes: the next start compares sequence numbers only (`missing_records`); `segment` / `length` appear only in a message.
Results: the F-07 unit tests: 26 of 27 pass, the one failure is the pinned assertion above; real binary, the three fixture-O phases with the F-11 assertion tightened to "the attestation is always present" and `F11_REPS=5`: **15 of 15 attested** (the shipped code on the same runs: 6 of 15 attested, 9 of 15 declined), all 15 rows exact, `rubixdb check` 0; the flush-abort stderr line still appears (4 of 15), because the engine is unchanged (`E:\rubixdb_f11c\logs\oi04_prototype_O_reps5.log`; the shipped-code baseline is `addendum_O_reps5_repo.log`).

## 4. Authorisation needed
The maintainer (owner of ADR-WAL-01) decides: amend the eligibility rule as in section 3 **and** replace the "rotated" case of that one unit test with its new expected outcome (plus a test that a header-only newest segment after a *torn* or *corrupt* older one is still refused). That is an F-07 change, in `src/ops/wal_tail.rs` and its tests, not a protected path. Until then OI-F11-04 stays OPEN: **NOT FIXED**, owner `src/ops/wal_tail.rs` (rule) with a cosmetic engine item (the abort is reported as "flush attempt 1 failed", which reads like a fault; `src/lsm`), priority P2.

## 5. What stays true meanwhile
The F-11 test assertion stays exactly as it is ("attested, or the stop says why"; mutation-proven non-vacuous, `addendum_O_reps5_mutated.log`); data outcome unchanged; the decline is never silent.

## 6. Authorisation (written)

Shape A is AUTHORISED for OI-F11-04.

Authorised by: Manik  
Role: Admin  
Date: 2026-10-11

Scope: Exactly the Shape A predicate change and the single header-only assertion reversal described in this ADR. Torn, corrupt, length-mismatch, 25+-byte newest segment, and header-only-after-a-torn-older-segment cases must continue to refuse attestation.

No other rule, test, or protected path change is authorised.

*Shape text (the maintainer's message of 2026-10-11, identical to `PHASE_ITEM_F07_OI04_ADR.md` section 4):* the attestation is also eligible when `records_replayed > 0`, `lvp.seq > 0`, the last-valid segment exists, its file length equals `lvp.offset`, and every newer segment is exactly 24 bytes; attest `{segment: lvp.segment_id, length: lvp.offset, last_seq: lvp.seq}`. The header-only case of `the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory` is replaced by "eligible". Scope: exactly the predicate change and the one assertion reversal; no change to the F-07 refusal path, the quarantine, the check-path fix, the cleanup sweep, the F-11 startup grace, or any other file. *Recorded in this file by the session on the maintainer's instruction; the name, role and date are the maintainer's, as given.*
