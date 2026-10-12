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

/// The largest bloom bit array, in bytes, that the reader accepts: the `256 * 1024 * 1024` limit that
/// `format::decode_bloom_block` applies to `ceil(num_bits / 8)` (the bit array only, not the 8-byte `num_bits`, the 1-byte
/// hash count or the 4-byte checksum around it). A table whose bit array is larger is refused with
/// `EngineError::CapacityExceeded`. This constant mirrors that literal; the test
/// `the_reader_cap_constant_matches_decode_bloom_block` fails if the two ever differ.
pub const MAX_BLOOM_BIT_ARRAY_BYTES: u64 = 256 * 1024 * 1024;

/// The bit count `BloomFilter::new_for_key_count` allocates for `expected_keys` at `bits_per_key`: the saturating product,
/// at least 1. The single formula used by the writer's sizing and by [`max_record_count_for_bloom_cap`].
pub const fn bloom_num_bits(expected_keys: u64, bits_per_key: u32) -> u64 {
    let n = expected_keys.saturating_mul(bits_per_key as u64);
    if n == 0 {
        1
    } else {
        n
    }
}

/// The largest footer `record_count` for which a bloom filter sized from it (`bloom_num_bits(record_count, bits_per_key)`)
/// still fits [`MAX_BLOOM_BIT_ARRAY_BYTES`], the reader's cap. Constant time, no I/O, no allocation.
///
/// Derivation: the bit array has `ceil(num_bits / 8)` bytes, and `ceil(n / 8) <= C` holds exactly when `n <= 8 * C`. So the
/// bound is the largest `r` with `r * bits_per_key <= 8 * C`, that is `floor(8 * C / bits_per_key)`. With the pinned
/// 10 bits per key and `C = 268_435_456` that is `floor(2_147_483_648 / 10) = 214_748_364`: that value needs
/// 2_147_483_640 bits = 268_435_455 bytes (accepted); the next, 214_748_365, needs 2_147_483_650 bits = 268_435_457 bytes
/// (refused by the reader). `8 * C = 2^31` fits `u64` with no overflow, and the division cannot overflow. `bits_per_key == 0`
/// sizes a 1-bit filter whatever the count, so every count is safe (`u64::MAX`).
pub const fn max_record_count_for_bloom_cap(bits_per_key: u32) -> u64 {
    if bits_per_key == 0 {
        return u64::MAX;
    }
    (MAX_BLOOM_BIT_ARRAY_BYTES * 8) / bits_per_key as u64
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
        let num_bits = bloom_num_bits(expected_keys, bits_per_key);
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
    fn the_largest_safe_record_count_at_the_default_is_exactly_214748364() {
        assert_eq!(
            max_record_count_for_bloom_cap(DEFAULT_BLOOM_BITS_PER_KEY),
            214_748_364
        );
        // The bound itself fits the reader's cap, the next integer does not.
        let at = bloom_num_bits(214_748_364, DEFAULT_BLOOM_BITS_PER_KEY);
        let beyond = bloom_num_bits(214_748_365, DEFAULT_BLOOM_BITS_PER_KEY);
        assert_eq!(at, 2_147_483_640);
        assert_eq!(at.div_ceil(8), 268_435_455);
        assert!(at.div_ceil(8) <= MAX_BLOOM_BIT_ARRAY_BYTES);
        assert_eq!(beyond, 2_147_483_650);
        assert_eq!(beyond.div_ceil(8), 268_435_457);
        assert!(beyond.div_ceil(8) > MAX_BLOOM_BIT_ARRAY_BYTES);
    }

    #[test]
    fn the_bound_is_exact_against_the_sizing_formula_for_many_bits_per_key() {
        for bpk in [
            1u32,
            2,
            3,
            7,
            8,
            9,
            10,
            11,
            16,
            17,
            64,
            1000,
            65_536,
            1 << 20,
        ] {
            let max = max_record_count_for_bloom_cap(bpk);
            assert!(
                bloom_num_bits(max, bpk).div_ceil(8) <= MAX_BLOOM_BIT_ARRAY_BYTES,
                "bpk {bpk}: the bound {max} must fit"
            );
            assert!(
                bloom_num_bits(max + 1, bpk).div_ceil(8) > MAX_BLOOM_BIT_ARRAY_BYTES,
                "bpk {bpk}: {} must not fit",
                max + 1
            );
        }
    }

    #[test]
    fn the_helper_arithmetic_does_not_overflow_at_the_extremes() {
        // bits_per_key 0 sizes a 1-bit filter: every count is safe.
        assert_eq!(max_record_count_for_bloom_cap(0), u64::MAX);
        assert_eq!(bloom_num_bits(u64::MAX, 0), 1);
        // The largest bits_per_key leaves no room for even one record.
        assert_eq!(max_record_count_for_bloom_cap(u32::MAX), 0);
        assert!(bloom_num_bits(1, u32::MAX).div_ceil(8) > MAX_BLOOM_BIT_ARRAY_BYTES);
        // The sizing product saturates instead of wrapping.
        assert_eq!(bloom_num_bits(u64::MAX, 10), u64::MAX);
        assert_eq!(bloom_num_bits(0, 10), 1);
        // 8 * C is 2^31: the numerator of the bound never overflows u64.
        assert_eq!(MAX_BLOOM_BIT_ARRAY_BYTES * 8, 1u64 << 31);
        assert_eq!(max_record_count_for_bloom_cap(1), 1u64 << 31);
    }

    #[test]
    fn the_reader_cap_constant_matches_decode_bloom_block() {
        use crate::error::EngineError;
        use crate::sstable::format::{decode_bloom_block, encode_bloom_block};
        let cap = MAX_BLOOM_BIT_ARRAY_BYTES as usize;
        // A bit array of exactly the cap decodes; one byte more is refused as CapacityExceeded.
        let at = encode_bloom_block(cap as u64 * 8, 7, &vec![0u8; cap]);
        assert!(decode_bloom_block(&at).is_ok());
        let over = encode_bloom_block(cap as u64 * 8 + 1, 7, &vec![0u8; cap + 1]);
        match decode_bloom_block(&over) {
            Err(EngineError::CapacityExceeded { requested, max }) => {
                assert_eq!(max, MAX_BLOOM_BIT_ARRAY_BYTES);
                assert_eq!(requested, MAX_BLOOM_BIT_ARRAY_BYTES + 1);
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

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
