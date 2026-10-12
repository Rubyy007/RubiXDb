p = "src/sstable/bloom.rs"
s = open(p, encoding="utf-8", newline="").read()
crlf = "\r\n" in s
s = s.replace("\r\n", "\n")

old_new = '''    pub fn new_for_key_count(expected_keys: u64, bits_per_key: u32) -> Self {
        let num_bits = (expected_keys.saturating_mul(bits_per_key as u64)).max(1);
        let num_bytes = num_bits.div_ceil(8) as usize;'''
new_new = '''    pub fn new_for_key_count(expected_keys: u64, bits_per_key: u32) -> Self {
        let num_bits = bloom_num_bits(expected_keys, bits_per_key);
        let num_bytes = num_bits.div_ceil(8) as usize;'''
assert s.count(old_new) == 1
s = s.replace(old_new, new_new)

anchor = "#[derive(Debug)]\npub struct BloomFilter {"
assert s.count(anchor) == 1
helper = '''/// The largest bloom bit array, in bytes, that the reader accepts: the `256 * 1024 * 1024` limit that
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

'''
s = s.replace(anchor, helper + anchor)

tests_anchor = "    #[test]\n    fn zero_false_negatives_over_every_inserted_key() {"
assert s.count(tests_anchor) == 1
tests = '''    #[test]
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
        for bpk in [1u32, 2, 3, 7, 8, 9, 10, 11, 16, 17, 64, 1000, 65_536, 1 << 20] {
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

'''
s = s.replace(tests_anchor, tests + tests_anchor)
if crlf: s = s.replace("\n", "\r\n")
open(p, "w", encoding="utf-8", newline="").write(s)

p = "src/sstable/reader.rs"
s = open(p, encoding="utf-8", newline="").read()
crlf = "\r\n" in s
s = s.replace("\r\n", "\n")
old = '''        let bloom_len_usize: usize = footer
            .bloom_length
            .try_into()
            .map_err(|_| corrupt("footer: bloom_length implausibly large"))?;'''
assert s.count(old) == 1
new = '''        // F-08 / GAP-B: the footer `record_count` is not otherwise validated at open, yet compaction sums it into the
        // output bloom size. Refuse, in constant time and before the bloom block is read or allocated, a count whose
        // bloom the reader itself would refuse (`bloom::max_record_count_for_bloom_cap`, derived from the reader cap).
        // The format is not changed; an honest table never exceeds it (its own bloom would not decode).
        let max_record_count = bloom::max_record_count_for_bloom_cap(DEFAULT_BLOOM_BITS_PER_KEY);
        if footer.record_count > max_record_count {
            return Err(corrupt(format!(
                "footer: record_count {} exceeds {max_record_count}, the largest count whose bloom filter fits the {} byte reader limit",
                footer.record_count,
                bloom::MAX_BLOOM_BIT_ARRAY_BYTES
            )));
        }

''' + old
s = s.replace(old, new)
imp = "use crate::sstable::bloom::BloomFilter;\nuse crate::sstable::format::{self, DecodedRecord, Footer, IndexEntry, FOOTER_SIZE, OP_PUT};"
assert s.count(imp) == 1
s = s.replace(imp, "use crate::sstable::bloom::{self, BloomFilter};\nuse crate::sstable::format::{\n    self, DecodedRecord, Footer, IndexEntry, DEFAULT_BLOOM_BITS_PER_KEY, FOOTER_SIZE, OP_PUT,\n};")
if crlf: s = s.replace("\n", "\r\n")
open(p, "w", encoding="utf-8", newline="").write(s)
print("patched")
