//! Engine-local, crash-safe record of the live SSTable set and last
//! checkpoint. Reuses the WAL's exact frame format (WAL Spec §2.3;
//! LSM Engine Spec §6).
//!
//! Not yet implemented — Phase 0, Step 5. Required infrastructure for the
//! SSTable atomic-construction discipline (LSM Engine Spec §3) and
//! Compaction (Step 6), not optional scaffolding.
