//! In-memory `(user_key, seq)`-ordered map with tombstone semantics, per
//! LSM Engine Spec Section 1: `get_as_of` via range + `next_back`, size
//! accounting, and the compile-time-enforced `freeze()` pattern.
//!
//! Not yet implemented — Phase 0, Step 2.
