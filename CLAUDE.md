# rubiXDb — Claude Code Rules

## Identity

Product: rubiXDb (exact casing). Binary: rubixdb. GUI: rubixdb gui.
CLI: rubixdb cli. Default: 127.0.0.1:302.
Local single-node relational database unless a mission authorizes otherwise.

## Priority Order

correctness > durability > recovery > security > resource safety > latency > throughput

## Source of Truth

Current repository is authoritative over chat history, summaries, and memory.
Inspect source before implementing. If docs conflict with source, stop and report.

## One Mission Per Session

Read CLAUDE.md, docs/PROJECT_STATE.md, missions/ACTIVE.md.
Execute that one mission. Stop at its stop condition.
Do not begin the next phase. Do not combine missions.

## Scope Discipline

If you notice a problem outside the mission:

* append one line to OPEN_ITEMS.md
* do not fix it
* do not discuss it
* continue

If a task feels adjacent, it is out of scope.

## Never Guess

Never invent requirements, architecture, numbers, guarantees, or state.
If information is missing: inspect, measure, or stop.

## Certification Rule

PASS requires: implementation + test + reproducible evidence + documentation.
Performance PASS additionally requires: workload, methodology, multiple runs,
latency distribution, resource measurements.
If evidence is missing: OPEN. If a test fails: FAIL.
Never convert OPEN→PASS or FAIL→PASS without resolving the issue.

## Protected Engine Boundary

Certified: src/wal/, src/manifest/, src/sstable/, src/compaction/,
and anything listed in docs/PROJECT_STATE.md as certified.
May be read, profiled, instrumented. May not be casually modified.
If a mission proves one must change: STOP, write an ADR, then proceed only
if the mission explicitly authorizes the engine change.

## Durability & Crash Rules

Never acknowledge durability before the contract is met.
Never weaken ordering, flush, or I/O error handling for throughput.
Never call graceful shutdown a crash. Never call process-kill a power loss.
Power-loss = NOT TESTED if untested.

## Security Is Always On

Preserve input validation, filesystem safety, resource limits, safe errors,
credential protection, destructive-action protection, SQLi/XSS resistance,
CLI terminal safety, instance isolation, loopback restriction.
Never disable security to improve performance or simplify testing.

## Do Not Weaken Tests

Never change tests to pass. Fix the test only if the test is proven wrong.
Document why.

## Git Discipline

Before work: git status, git branch, git rev-parse HEAD.
Do not overwrite unrelated user changes. Do not reset without authorization.
Commit locally per sub-step. Report the hash. Do not push unless asked.

## Documentation Discipline

PROGRESS.md and CHANGELOG.md are append-only.
Never rewrite historical certification documents.
Mark superseded docs as SUPERSEDED, keep the originals.

## Stop Conditions

STOP if: a required measurement cannot be performed; tooling is unavailable;
source contradicts mission assumptions; a certified component must change
unexpectedly; durability/recovery/security would weaken; scope exceeds the
mission; correctness cannot be proven; acceptance criteria are ambiguous.
Report the exact reason. Do not guess.

## Mission Completion

End every mission with a report containing:
Implemented / Measured / Verified / Passed / Failed / Open / Not Tested /
Protected Paths / Documentation / Git hash.
Then STOP. Do not begin the next phase.

## Production Readiness

Do not declare rubiXDb PRODUCTION READY until the current mandatory
certification matrix is entirely PASS — no mandatory FAIL, OPEN, or hidden
NOT TESTED.
