//! RUBIC SSTable bloom filter, per
//! `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.5: 10 bits/key, 7 hash
//! functions, two independent XXH64 hashes (seeds 0 and 1) combined by
//! Kirsch-Mitzenmacher double hashing. Never produces a false negative
//! (operating brief §17) — `might_contain` returning `false` is always a
//! definite answer; `true` only ever means "maybe, go check the block."

use xxhash_rust::xxh64::xxh64;

use crate::sstable::format::DEFAULT_BLOOM_BITS_PER_KEY;

/// `round(bits_per_key * ln(2))`, per the spec's own formula. For the
/// pinned default (10 bits/key) this is `round(6.931...) = 7`.
pub fn num_hash_functions_for(bits_per_key: u32) -> u8 {
    let n = (bits_per_key as f64 * std::f64::consts::LN_2).round();
    // Clamped to at least 1 and to u8::MAX -- bits_per_key is a small,
    // project-controlled config value, never attacker-controlled, but the
    // clamp keeps this total regardless.
    n.clamp(1.0, u8::MAX as f64) as u8
}

#[derive(Debug)]
pub struct BloomFilter {
    num_bits: u64,
    num_hash_functions: u8,
    bits: Vec<u8>,
}

impl BloomFilter {
    /// Sizes the bit array for `expected_keys` insertions at
    /// `bits_per_key` bits each (§2.4 — the writer knows the frozen
    /// MemTable's exact `entry_count()` up front, so this is precise, not
    /// a rough estimate). `expected_keys == 0` still allocates a minimal,
    /// valid (1-bit) filter rather than a degenerate zero-size one.
    pub fn new_for_key_count(expected_keys: u64, bits_per_key: u32) -> Self {
        let num_bits = (expected_keys.saturating_mul(bits_per_key as u64)).max(1);
        let num_bytes = num_bits.div_ceil(8) as usize;
        BloomFilter {
            num_bits,
            num_hash_functions: num_hash_functions_for(bits_per_key),
            bits: vec![0u8; num_bytes],
        }
    }

    /// Reconstructs a filter from already-validated, decoded components
    /// (reader path — `format::decode_bloom_block` has already checksum-
    /// verified `bits` before this is ever called).
    pub fn from_parts(num_bits: u64, num_hash_functions: u8, bits: Vec<u8>) -> Self {
        BloomFilter {
            num_bits,
            num_hash_functions,
            bits,
        }
    }

    pub fn num_bits(&self) -> u64 {
        self.num_bits
    }

    pub fn num_hash_functions(&self) -> u8 {
        self.num_hash_functions
    }

    pub fn bits(&self) -> &[u8] {
        &self.bits
    }

    fn hash_pair(key: &[u8]) -> (u64, u64) {
        (xxh64(key, 0), xxh64(key, 1))
    }

    fn bit_positions(&self, key: &[u8]) -> impl Iterator<Item = u64> + '_ {
        let (h1, h2) = Self::hash_pair(key);
        let num_bits = self.num_bits;
        (0..self.num_hash_functions as u64).map(move |i| {
            // Wrapping arithmetic is intentional and harmless here: this
            // only selects a bit position via modulo, never sizes an
            // allocation or a read -- CapacityExceeded/overflow checks
            // elsewhere in this module are what actually guard memory
            // safety (operating brief §45).
            h1.wrapping_add(i.wrapping_mul(h2)) % num_bits
        })
    }

    /// Idempotent under repeated insertion of the same key (spec §2.4's
    /// own note) — every record's key is added once per occurrence,
    /// including repeated versions of the same user key, with no dedup
    /// pass needed.
    pub fn insert(&mut self, key: &[u8]) {
        let positions: Vec<u64> = self.bit_positions(key).collect();
        for bit in positions {
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            self.bits[byte] |= mask;
        }
    }

    /// `false` = definitely absent (zero I/O needed); `true` = maybe
    /// present (caller must still check the real data). Never a false
    /// negative for any key actually inserted.
    pub fn might_contain(&self, key: &[u8]) -> bool {
        self.bit_positions(key).all(|bit| {
            let byte = (bit / 8) as usize;
            let mask = 1u8 << (bit % 8);
            self.bits[byte] & mask != 0
        })
    }
}

impl Default for BloomFilter {
    fn default() -> Self {
        BloomFilter::new_for_key_count(0, DEFAULT_BLOOM_BITS_PER_KEY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_false_negatives_over_every_inserted_key() {
        let mut bf = BloomFilter::new_for_key_count(1000, DEFAULT_BLOOM_BITS_PER_KEY);
        let keys: Vec<String> = (0..1000).map(|i| format!("key-{i:06}")).collect();
        for k in &keys {
            bf.insert(k.as_bytes());
        }
        for k in &keys {
            assert!(
                bf.might_contain(k.as_bytes()),
                "false negative for inserted key {k}"
            );
        }
    }

    #[test]
    fn false_positive_rate_within_reasonable_bound_of_target() {
        let n = 5000u64;
        let mut bf = BloomFilter::new_for_key_count(n, DEFAULT_BLOOM_BITS_PER_KEY);
        for i in 0..n {
            bf.insert(format!("present-{i}").as_bytes());
        }
        let mut false_positives = 0u64;
        let trials = 20_000u64;
        for i in 0..trials {
            if bf.might_contain(format!("absent-{i}").as_bytes()) {
                false_positives += 1;
            }
        }
        let rate = false_positives as f64 / trials as f64;
        // Target ~1% at 10 bits/key; assert well within an order of
        // magnitude (statistical test, not exact, per the spec's own
        // test-checklist item 2).
        assert!(rate < 0.05, "false positive rate too high: {rate}");
    }

    #[test]
    fn repeated_insertion_of_same_key_is_harmless() {
        let mut bf = BloomFilter::new_for_key_count(10, DEFAULT_BLOOM_BITS_PER_KEY);
        for _ in 0..5 {
            bf.insert(b"same-key");
        }
        assert!(bf.might_contain(b"same-key"));
    }

    #[test]
    fn empty_filter_never_panics() {
        let bf = BloomFilter::new_for_key_count(0, DEFAULT_BLOOM_BITS_PER_KEY);
        assert!(bf.num_bits() >= 1);
        // An empty filter may return true or false for any given key
        // (undefined which, since nothing was ever inserted) but must
        // never panic.
        let _ = bf.might_contain(b"anything");
    }

    #[test]
    fn num_hash_functions_matches_spec_formula_for_default() {
        assert_eq!(num_hash_functions_for(10), 7);
    }
}
