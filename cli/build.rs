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
    windows_resources(&manifest);
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

// ---------------------------------------------------------------------------
// Windows executable resources: the application icon and the version
// information (the "Details" tab of the file's Properties). Without them
// Explorer shows the generic blank-application icon and an empty Details tab,
// which looks like an unknown, untrustworthy program.
//
// The resource table (a `.res` file) is written here byte for byte instead of
// by `rc.exe` or a build-dependency crate: it needs no Windows SDK on the build
// machine, adds no dependency to audit, and is deterministic, so the
// reproducible release build (`scripts/release.ps1`, `/Brepro`) stays
// reproducible. The icon is `cli/assets/rubixdb.ico`, made from the same mark as
// the browser-tab icon (`frontend/public/icon-512.png`).
// ---------------------------------------------------------------------------

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const LANG_EN_US: u16 = 0x0409;

fn pad4(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// One entry of a `.res` file: ordinal type and name, neutral data.
fn res_entry(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes()); // header size
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(id.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // data version
    out.extend(0x0030u16.to_le_bytes()); // memory flags: moveable, pure
    out.extend(LANG_EN_US.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // version
    out.extend(0u32.to_le_bytes()); // characteristics
    out.extend_from_slice(data);
    pad4(out);
}

/// One node of a VS_VERSIONINFO tree. Its length field excludes the padding that follows it; padding
/// between a node and its next sibling is added by the parent.
fn ver_block(key: &str, value_len: u16, text: bool, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = vec![0, 0];
    b.extend(value_len.to_le_bytes());
    b.extend(u16::from(text).to_le_bytes());
    b.extend(utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for c in children {
        pad4(&mut b);
        b.extend_from_slice(c);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn version_info(version: &str) -> Vec<u8> {
    let nums: Vec<u16> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .chain(std::iter::repeat(0))
        .take(4)
        .collect();
    let (ms, ls) = (
        (u32::from(nums[0]) << 16) | u32::from(nums[1]),
        (u32::from(nums[2]) << 16) | u32::from(nums[3]),
    );
    let mut fixed = Vec::new();
    for v in [
        0xFEEF_04BDu32, // signature
        0x0001_0000,    // structure version
        ms,
        ls,
        ms,
        ls,
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,
        0,
        0,
    ] {
        fixed.extend(v.to_le_bytes());
    }
    let full = format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3]);
    let strings: Vec<(&str, String)> = vec![
        ("CompanyName", "rubiXDb".to_string()),
        (
            "FileDescription",
            "rubiXDb - local relational database (CLI, console and server)".to_string(),
        ),
        ("FileVersion", full.clone()),
        ("InternalName", "rubixdb".to_string()),
        ("OriginalFilename", "rubixdb.exe".to_string()),
        ("ProductName", "rubiXDb".to_string()),
        ("ProductVersion", full),
    ];
    let entries: Vec<Vec<u8>> = strings
        .iter()
        .map(|(k, v)| {
            let value = utf16z(v);
            ver_block(k, (value.len() / 2) as u16, true, &value, &[])
        })
        .collect();
    let table = ver_block("040904B0", 0, true, &[], &entries);
    let string_info = ver_block("StringFileInfo", 0, true, &[], &[table]);
    let translation = [0x09, 0x04, 0xB0, 0x04];
    let var = ver_block("Translation", 4, false, &translation, &[]);
    let var_info = ver_block("VarFileInfo", 0, true, &[], &[var]);
    ver_block(
        "VS_VERSION_INFO",
        fixed.len() as u16,
        false,
        &fixed,
        &[string_info, var_info],
    )
}

fn windows_resources(manifest: &Path) {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" {
        return;
    }
    if target_env != "msvc" {
        println!(
            "cargo:warning=executable icon and version info are only embedded for the MSVC target"
        );
        return;
    }
    let ico_path = manifest.join("assets").join("rubixdb.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());
    let ico = fs::read(&ico_path).expect("cli/assets/rubixdb.ico must exist");
    assert!(
        ico.len() >= 6 && ico[2] == 1 && ico[3] == 0,
        "rubixdb.ico is not an .ico file"
    );
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;

    let mut res = Vec::new();
    // A .res file starts with an empty 32-byte resource.
    res.extend(0u32.to_le_bytes());
    res.extend(32u32.to_le_bytes());
    res.extend([0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0]);
    res.extend([0u8; 16]);

    let mut group = Vec::new();
    group.extend([0, 0, 1, 0]);
    group.extend((count as u16).to_le_bytes());
    for i in 0..count {
        let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
        let size = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
        let id = (i + 1) as u16;
        res_entry(&mut res, RT_ICON, id, &ico[offset..offset + size]);
        group.extend_from_slice(&e[0..8]); // width, height, colours, reserved, planes, bit count
        group.extend((size as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    // Group id 1 is the lowest, which is the icon Explorer shows for the .exe.
    res_entry(&mut res, RT_GROUP_ICON, 1, &group);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    res_entry(&mut res, RT_VERSION, 1, &version_info(&version));

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("rubixdb.res");
    fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
