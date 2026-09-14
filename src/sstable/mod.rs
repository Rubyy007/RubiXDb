//! Immutable on-disk sorted structure: data blocks, bloom filter block,
//! sparse index block, and a fixed 72-byte footer, per LSM Engine Spec
//! Section 2. Built via the atomic tmp-file-then-rename discipline in
//! Section 3.
//!
//! Not yet implemented — Phase 0, Step 3.
