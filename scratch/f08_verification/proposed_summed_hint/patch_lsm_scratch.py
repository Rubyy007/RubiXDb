"""SCRATCH-ONLY patch (applied to E:\\rubixdb_f08b\\nv_fix, never to the repository): the summed-hint clamp, the pre-commit output
guard, the one-line re-export that makes the helper reachable from src/lsm, and the unit tests of the two pure functions."""
import sys
ROOT = sys.argv[1]


def edit(rel, fn):
    p = ROOT + "/" + rel
    s = open(p, encoding="utf-8", newline="").read()
    crlf = "\r\n" in s
    s = fn(s.replace("\r\n", "\n"))
    open(p, "w", encoding="utf-8", newline="").write(s.replace("\n", "\r\n") if crlf else s)


def mod_rs(s):
    old = "pub use bloom::BloomFilter;"
    assert s.count(old) == 1
    return s.replace(old, "pub use bloom::{max_record_count_for_bloom_cap, BloomFilter};")


def lsm(s):
    old_use = "use crate::sstable::{\n    self, RecordValue, SsTable, SsTableRangeCursor, SsTableWriterConfig, SstableMeta,\n};"
    assert s.count(old_use) == 1
    s = s.replace(old_use, "use crate::sstable::format::DEFAULT_BLOOM_BITS_PER_KEY;\nuse crate::sstable::{\n    self, max_record_count_for_bloom_cap, RecordValue, SsTable, SsTableRangeCursor,\n    SsTableWriterConfig, SstableMeta,\n};")

    old_hint = "    let entry_count_hint: u64 = inputs.iter().map(|t| t.record_count()).sum();\n"
    assert s.count(old_hint) == 1
    s = s.replace(old_hint, '''    // F-08 / GAP-B: the hint sizes the output bloom and nothing else (`write_from_sorted_records` inserts every record's key
    // and counts every record whatever the hint is). The sum of the inputs' counts can exceed the largest count whose bloom
    // the reader accepts, so it is summed without wrapping and then CAPPED at that count: the output bloom is intentionally
    // no larger than the reader will read, which is exactly as large as a table of that many records needs.
    let bloom_count_cap = compaction_record_cap(bloom_bits_per_key);
    let entry_count_hint: u64 = capped_bloom_hint(inputs.iter().map(|t| t.record_count()), bloom_count_cap);
''')

    old_meta = "    let output_bytes = std::fs::metadata(&output_meta.path)\n        .map(|m| m.len())\n        .unwrap_or(0);\n    let merge_stats = *merge_stats_handle"
    assert s.count(old_meta) == 1
    s = s.replace(old_meta, '''    // F-08 / GAP-B: refuse BEFORE the Manifest commit an output the reader would refuse to open (its footer count above the
    // bound `SsTable::open` enforces). Nothing references the file yet, so it is removed; the error is a permanent-candidate
    // class (`Unsupported`), so the worker blocks after its budget instead of retrying forever.
    if !compaction_output_fits(output_meta.record_count, bloom_count_cap) {
        let _ = std::fs::remove_file(&output_meta.path);
        return Err(EngineError::Unsupported {
            operation: format!(
                "compaction output of {} records (a table holds at most {bloom_count_cap} records, the most a bloom filter within the reader limit can be sized for); not committed",
                output_meta.record_count
            ),
        });
    }
''' + old_meta)

    anchor = "#[allow(clippy::too_many_arguments)]\nfn compact_once_impl("
    assert s.count(anchor) == 1
    helpers = '''/// The per-table record bound the compaction output must respect: the larger of the configured and the format-pinned bits per
/// key, so it is never above what `SsTable::open` accepts (`sstable::bloom::max_record_count_for_bloom_cap`).
fn compaction_record_cap(bloom_bits_per_key: u32) -> u64 {
    max_record_count_for_bloom_cap(bloom_bits_per_key.max(DEFAULT_BLOOM_BITS_PER_KEY))
}

/// Sum of the inputs' footer counts, saturating (a wrapped sum could become a deceptively small hint), then capped at `cap`.
fn capped_bloom_hint(counts: impl Iterator<Item = u64>, cap: u64) -> u64 {
    counts.fold(0u64, |a, c| a.saturating_add(c)).min(cap)
}

/// `true` when an output holding `record_count` records is a table the reader will open.
fn compaction_output_fits(record_count: u64, cap: u64) -> bool {
    record_count <= cap
}

'''
    # insert above the doc comment of `compact_once_impl` (not between the comment and the attribute)
    b = s.index(anchor)
    lines = s[:b].split(chr(10))
    j = len(lines) - 2
    while lines[j].startswith("///"):
        j -= 1
    start = len(chr(10).join(lines[: j + 1])) + 1
    s = s[:start] + helpers + s[start:]

    s = s.rstrip("\n") + '''

#[cfg(test)]
mod summed_hint_tests {
    use super::*;

    const CAP: u64 = 214_748_364;

    #[test]
    fn the_cap_is_the_per_table_bound_at_the_default_and_never_above_it_for_other_bits_per_key() {
        assert_eq!(compaction_record_cap(DEFAULT_BLOOM_BITS_PER_KEY), CAP);
        assert_eq!(compaction_record_cap(0), CAP);
        assert_eq!(compaction_record_cap(1), CAP);
        assert!(compaction_record_cap(20) < CAP);
    }

    #[test]
    fn the_hint_is_the_plain_sum_below_the_cap_and_the_cap_above_it() {
        assert_eq!(capped_bloom_hint([10u64, 20, 30].into_iter(), CAP), 60);
        assert_eq!(capped_bloom_hint([CAP].into_iter(), CAP), CAP);
        assert_eq!(capped_bloom_hint([CAP - 5, 5].into_iter(), CAP), CAP);
        assert_eq!(capped_bloom_hint([CAP - 5, 6].into_iter(), CAP), CAP);
        assert_eq!(capped_bloom_hint([CAP, 1, 1, 1].into_iter(), CAP), CAP);
        assert_eq!(capped_bloom_hint(std::iter::empty(), CAP), 0);
    }

    #[test]
    fn the_sum_saturates_instead_of_wrapping_into_a_small_hint() {
        // u64::MAX + 2 would wrap to 1 and size a one-bit bloom for an enormous table.
        assert_eq!(capped_bloom_hint([u64::MAX, 2].into_iter(), CAP), CAP);
        assert_eq!(capped_bloom_hint([u64::MAX, u64::MAX].into_iter(), CAP), CAP);
    }

    #[test]
    fn an_output_above_the_cap_does_not_fit_and_the_cap_itself_does() {
        assert!(compaction_output_fits(0, CAP));
        assert!(compaction_output_fits(CAP, CAP));
        assert!(!compaction_output_fits(CAP + 1, CAP));
        assert!(!compaction_output_fits(u64::MAX, CAP));
    }
}
'''
    return s


edit("src/sstable/mod.rs", mod_rs)
edit("src/lsm/mod.rs", lsm)
print("scratch patch applied to", ROOT)
