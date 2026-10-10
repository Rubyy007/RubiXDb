# ADR-F07-OI04 — attestation eligibility for a header-only newest WAL segment (OI-F11-04): DECISION REQUEST, NOT AUTHORISED, NOT IMPLEMENTED

**Date:** 2026-10-10. **Status:** PROPOSED. This is the decision package that the F-11 OI-F11-04 workstream stopped on. Its gate was not met: `PHASE_ITEM_F11_OI04_ADR.md` records a PROPOSAL ("Authorisation needed ... the maintainer (owner of ADR-WAL-01) decides"), not a written authorisation of Shape A. No source or test was changed for OI-F11-04. **Answer needed (one line):** "Shape A authorised: yes / no / Shape ___ instead."

## 1. Context (reproduced; evidence in `PHASE_ITEM_F11_OI04_ADR.md` and `E:\rubixdb_f11c\logs\addendum_O_reps5_repo.log`)
A graceful stop during the start's own recovery flush aborts the flush after `pool.rotate()` created the next WAL segment (header only, 24 bytes). `write_attestation` (`src/ops/wal_tail.rs`) answers `NotEligible("the replay summary does not match the newest segment ...")`. Shipped code, fixture O x5: 9 of 15 stops declined the attestation (earlier: 2 of 9, 4 of 9, 3 of 6); data intact every time.

## 2. The rule today, and the assertion that pins it
`consistent = lvp.segment_id == last_id && lvp.offset == len && len >= SEGMENT_HEADER_LEN && (records_replayed > 0 || lvp.seq == 0)` where `last_id` is the newest segment id. The unit test `the_attestation_is_not_written_for_a_corrupt_torn_or_inconsistent_directory` (`src/ops/wal_tail_tests.rs`, lines 373-386) pins this case: *"the newest segment is a header-only file after earlier ones: the final length check fails"* - a 4-record segment 1 plus a hand-written 24-byte segment 2, then `assert!(matches!(write_attestation(&rotated), AttestWrite::NotEligible(_)))` and `assert!(!rotated.join(ATTESTATION_FILE).exists())`.

## 3. Which states leave a header-only newest segment, and are they clean or damaged?
| Path | Acknowledged bytes lost? | Class |
|---|---|---|
| recovery flush aborted by the stop, after `rotate()` and before the marker record | no: the older segment ends exactly at its last valid record | clean |
| any flush/rotation followed by a stop before the first record of the new segment | no | clean |
| kill during rotation | n/a: a kill writes no attestation | not reachable by the attestation |
| the newest segment truncated by outside damage to its header while it held records | yes, but it happened before the stop and is undetectable by any attestation written afterwards | not distinguishable here (same for the shipped rule) |
| older segment torn or corrupt, newer header-only | yes | damaged; refused today by `summary.truncated` / `corrupted_segments_count`, **unchanged by Shape A** |

## 4. Choices
* **Shape A (proposed; measured on a scratch tree).** Eligible also when `records_replayed > 0`, `lvp.seq > 0`, the last-valid segment exists, its file length equals `lvp.offset`, and **every newer segment is exactly 24 bytes**; attest `{segment: lvp.segment_id, length: lvp.offset, last_seq: lvp.seq}`. The next start compares sequence numbers only (`missing_records`); `segment` and `length` appear only in a message. Evidence: F-07 unit tests 26 of 27 pass (the one failure is the pinned case); fixture O x5 with the F-11 assertion tightened to "always attested": 15 of 15 attested vs 6 of 15 shipped, rows exact, `rubixdb check` 0 (`oi04_prototype_O_reps5.log`). Consequence: the pinned test's header-only case flips to "eligible"; torn / corrupt / length-mismatch / extra-byte cases must stay refused and get their own assertions; the attestation reliably follows a clean stop.
* **Shape B.** Make the stop complete the flush instead of aborting it (engine shutdown ordering, `src/lsm`). Changes the data outcome (no orphan table), engine behaviour the earlier missions fenced off, stop latency; does not help a rotation without a flush.
* **Shape C.** Keep the rule; document that the attestation is not written when a stop races a recovery flush or a fresh rotation. No code risk; the decline stays (9 of 15 in the fixture-O test; its rate in real use was not measured) and every such stop is followed by an unattested start, which loses ADR-WAL-01's refusal for that start.

## 5. What an authorisation must state (so that the implementation is exactly bounded)
(1) Shape A as in section 4, verbatim. (2) The pinned case is replaced by "eligible, attests the older segment's end". (3) Retained refusals, each separately asserted: torn tail, corrupt segment, last-valid length mismatch, newest segment longer than its header (25+ bytes), header-only newest after a torn older segment. (4) No change to the `WAL_TAIL_DAMAGED` text, the startup guard, or any protected path. (5) Acceptance: `F11_REPS=5` on fixture O attests every time with rows exact; every unrelated case still refuses with its existing error class.

## 6. Failure model and risk
Process termination / graceful stop only; power loss NOT TESTED. Residual risk of A: an attestation that no longer names the newest segment could mask a file-system-level loss of an empty-looking newest segment's contents that happened before the stop (section 3, row 4), which the shipped rule also cannot detect.

## 7. Not done in this session
No source, test, or ADR rule changed; OI-F11-04 stays OPEN / NOT FIXED. Waiting for the answer in the first paragraph.
