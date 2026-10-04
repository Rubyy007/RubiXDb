//! Embeds the built console (`frontend/dist`, from `npm run build`) into the
//! `rubixdb` executable, so one file carries both the engine and the UI. No
//! dependency: this script writes a table of `include_bytes!` entries to
//! `$OUT_DIR/embedded_frontend.rs`. Source maps are skipped (not needed to
//! serve the console, and they would only add size). If `frontend/dist` has not
//! been built, the table is empty and the binary falls back to the on-disk
//! lookup in `src/frontend_dist.rs` -- build the frontend first for a
//! self-contained binary (`scripts/release.ps1` already does).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            collect(&p, base, out);
        } else if p.extension().is_none_or(|x| x != "map") {
            let rel = p
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, p));
        }
    }
}

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = manifest.join("..").join("frontend").join("dist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
    } else {
        println!(
            "cargo:warning=frontend/dist not built: rubixdb will not embed the console (run `npm run build` in frontend/ first)"
        );
    }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
        hash = fnv(hash, rel.as_bytes());
        hash = fnv(hash, &fs::read(path).unwrap_or_default());
        writeln!(
            src,
            "    ({rel:?}, include_bytes!({:?})),",
            abs.to_string_lossy().as_ref()
        )
        .unwrap();
    }
    src.push_str("];\n");
    writeln!(src, "pub const HASH: &str = \"{hash:016x}\";").unwrap();
    fs::write(
        PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("embedded_frontend.rs"),
        src,
    )
    .unwrap();
}
