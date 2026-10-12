import subprocess, re, os
ROOT = "E:/rubixdb_f08b/nv_fix"
L = ROOT + "/src/lsm/mod.rs"
M = ROOT + "/src/sstable/mod.rs"
env = dict(os.environ, CARGO_TARGET_DIR="E:/rubixdb_f08b/nv_fix_target")
fixed_l = open(L, encoding="utf-8", newline="").read()
fixed_m = open(M, encoding="utf-8", newline="").read()


def run():
    a = subprocess.run("cargo test -p rubixdb --test compaction_summed_hint", shell=True, cwd=ROOT, capture_output=True, text=True, env=env)
    b = subprocess.run("cargo test -p rubixdb --lib summed_hint", shell=True, cwd=ROOT, capture_output=True, text=True, env=env)
    ok = "could not compile" not in a.stderr and "could not compile" not in b.stderr
    return ok, re.findall(r"^test (\S+) \.\.\. FAILED", a.stdout, re.M), re.findall(r"^test (\S+) \.\.\. FAILED", b.stdout, re.M)


def mutate(name, old, new):
    s = fixed_l.replace("\r\n", "\n")
    assert s.count(old) == 1, name
    open(L, "w", encoding="utf-8", newline="").write(s.replace(old, new))
    ok, fa, fb = run()
    open(L, "w", encoding="utf-8", newline="").write(fixed_l)
    print(f"{name} | compiled {ok} | integration failures {fa} | unit failures {fb}", flush=True)


# unfixed: the compaction call site as at HEAD (helpers still present and unit-tested, but not used)
mutate("U  unfixed call site (sum, no cap, no guard)",
       "capped_bloom_hint(inputs.iter().map(|t| t.record_count()), bloom_count_cap);",
       "inputs.iter().map(|t| t.record_count()).sum();")
mutate("Ma clamp removed", ".fold(0u64, |a, c| a.saturating_add(c)).min(cap)", ".fold(0u64, |a, c| a.saturating_add(c))")
mutate("Mb clamp applied before the sum (per input), not after",
       "counts.fold(0u64, |a, c| a.saturating_add(c)).min(cap)",
       "counts.map(|c| c.min(cap)).fold(0u64, |a, c| a.saturating_add(c))")
mutate("Mc wrapping sum (overflow wraps into a small hint)", "a.saturating_add(c)", "a.wrapping_add(c)")
mutate("Md guard off by one (refuses the cap itself)", "record_count <= cap\n}", "record_count < cap\n}")
mutate("Me guard never refuses", "record_count <= cap\n}", "{ let _ = cap; let _ = record_count; true }\n}")
mutate("Mf guard call removed from the compaction path", "if !compaction_output_fits(output_meta.record_count, bloom_count_cap) {", "if false && !compaction_output_fits(output_meta.record_count, bloom_count_cap) {")
