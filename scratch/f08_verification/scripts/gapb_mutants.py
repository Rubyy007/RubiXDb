import subprocess, shutil, re, sys
R = "E:/rubixdb_f08b/nv/src/sstable/reader.rs"; B = "E:/rubixdb_f08b/nv/src/sstable/bloom.rs"
rf = open("E:/rubixdb_f08b/reader.fixed", encoding="utf-8", newline="").read()
bf = open("E:/rubixdb_f08b/bloom.fixed", encoding="utf-8", newline="").read()
def run():
    env = dict(__import__("os").environ, CARGO_TARGET_DIR="E:/rubixdb_f08b/nv_target")
    a = subprocess.run("cargo test -p rubixdb --test sstable_record_count_bound", shell=True, cwd="E:/rubixdb_f08b/nv", capture_output=True, text=True, env=env)
    b = subprocess.run("cargo test -p rubixdb --lib sstable::bloom", shell=True, cwd="E:/rubixdb_f08b/nv", capture_output=True, text=True, env=env)
    fa = re.findall(r"^test (\S+) \.\.\. FAILED", a.stdout, re.M); fb = re.findall(r"^test (\S+) \.\.\. FAILED", b.stdout, re.M)
    compiled = ("could not compile" not in a.stderr) and ("could not compile" not in b.stderr)
    return compiled, fa, fb
nl = "\r\n" if "\r\n" in rf else "\n"
M = [
 ("M1 reader: > becomes >= (rejects the bound itself)", R, rf, "if footer.record_count > max_record_count {", "if footer.record_count >= max_record_count {"),
 ("M2 reader: > becomes > max+1 (accepts the next integer)", R, rf, "if footer.record_count > max_record_count {", "if footer.record_count > max_record_count + 1 {"),
 ("M3 helper: divisor bits_per_key+1", B, bf, "(MAX_BLOOM_BIT_ARRAY_BYTES * 8) / bits_per_key as u64", "(MAX_BLOOM_BIT_ARRAY_BYTES * 8) / (bits_per_key as u64 + 1)"),
 ("M4 helper: cap in bits instead of bytes*8 (x4)", B, bf, "(MAX_BLOOM_BIT_ARRAY_BYTES * 8) / bits_per_key as u64", "(MAX_BLOOM_BIT_ARRAY_BYTES * 32) / bits_per_key as u64"),
 ("M5 sizing: bloom_num_bits loses the max(1)", B, bf, "    if n == 0 {\n        1\n    } else {\n        n\n    }", "    n"),
]
out = []
for name, path, orig, old, new in M:
    o = orig.replace("\r\n", "\n"); assert o.count(old) == 1, name
    m = o.replace(old, new)
    if "\r\n" in orig: m = m.replace("\n", "\r\n")
    open(path, "w", encoding="utf-8", newline="").write(m)
    ok, fa, fb = run()
    open(path, "w", encoding="utf-8", newline="").write(orig)
    out.append((name, ok, fa, fb)); print(name, "| compiled", ok, "| integration failures", fa, "| unit failures", fb, flush=True)
# M6: ordering -- the record_count check moved after the bloom block is decoded
o = rf.replace("\r\n", "\n")
a = o.index("        // F-08 / GAP-B:"); b = o.index("        let bloom_len_usize")
chk = o[a:b]; o2 = o[:a] + o[b:]
anchor = "        let index_len_usize: usize = footer"
assert o2.count(anchor) == 1
o2 = o2.replace(anchor, chk + anchor)
if "\r\n" in rf: o2 = o2.replace("\n", "\r\n")
open(R, "w", encoding="utf-8", newline="").write(o2)
ok, fa, fb = run(); open(R, "w", encoding="utf-8", newline="").write(rf)
print("M6 reader: check moved after the bloom block is read and decoded | compiled", ok, "| integration failures", fa, "| unit failures", fb, flush=True)
