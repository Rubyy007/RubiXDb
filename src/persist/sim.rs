//! `SimFs`: the power-loss simulator behind `crate::persist` (feature `sim-fs`; never in a product build).
//!
//! It is the executable form of `docs/PERSISTENCE_MODEL.md` (assumptions M1-M8). It records, per file, the volatile content,
//! the content as of the last successful `sync_all` and the ordered writes / length changes since; and, per directory, an
//! ordered log of namespace operations each of which becomes durable only after `sync_dir` on the directory(ies) it
//! affects. A *simulated power loss* is a pure function of `(dump, crash configuration, seed)` - it never touches the real
//! disk and never depends on thread timing (M8).
//!
//! Nothing here is a measurement of any device. Assumptions A1-A5 of `docs/DURABILITY_CONTRACT.md` are inherited and are
//! untested on this platform: the simulator makes them executable, it does not verify them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use super::{DirEntry, FileType, Metadata, OpenOptions, PFile, ReadDir, TryLockError};

type FileId = u64;

// ---------------------------------------------------------------------------------------------------------------------
// process-wide installation
// ---------------------------------------------------------------------------------------------------------------------

static ACTIVE: RwLock<Option<Arc<SimFs>>> = RwLock::new(None);

pub(crate) fn active() -> Option<Arc<SimFs>> {
    ACTIVE.read().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Routes every `crate::persist` call of this process to `sim` until the returned guard is dropped.
pub fn install(sim: Arc<SimFs>) -> SimGuard {
    *ACTIVE.write().unwrap_or_else(|p| p.into_inner()) = Some(sim);
    SimGuard(())
}

/// Uninstalls the simulator on drop.
pub struct SimGuard(());

impl Drop for SimGuard {
    fn drop(&mut self) {
        *ACTIVE.write().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// configuration and crash policies
// ---------------------------------------------------------------------------------------------------------------------

/// Runtime behaviour of the simulated device (not of a crash).
#[derive(Debug, Clone, Copy)]
pub struct SimConfig {
    /// `true`: `sync_dir` makes the affected namespace operations durable (the Unix behaviour of `RealFs`). `false`: it does
    /// nothing, as on Windows (A5): namespace operations are then never durable by themselves.
    pub dir_sync_effective: bool,
    /// Sector size in bytes (torn-write granularity, A2).
    pub sector: usize,
    /// M7: if `Some((seed, per_million))`, that fraction of `sync_all` calls returns `Ok` without making anything durable
    /// (the device violates A1). Off by default; used only by the dedicated fsync-lie test.
    pub fsync_lie: Option<(u64, u32)>,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            dir_sync_effective: false,
            sector: 512,
            fsync_lie: None,
        }
    }
}

/// What happens to writes that were not covered by a `sync_all` (M1, M3, M4, M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataPolicy {
    /// M1 in its strict form: every unsynced write and length change is lost.
    DiscardUnsynced,
    /// Crash class B (a process kill, NOT a power loss): the OS keeps every write the process completed, synced or not. No
    /// part of the power-loss model applies; used by the kill-9 variant of the cycle.
    KeepAll,
    /// Any subset may persist. `reorder = false`: only a prefix of the unsynced writes (in order). `reorder = true`: any
    /// subset (M4). `tear`: the last sector of the last persisted write may be torn (M3). `length_before_data`: the length
    /// of an unsynced extension may persist without its data (M5: a zero-filled tail).
    Random {
        reorder: bool,
        tear: bool,
        length_before_data: bool,
    },
}

/// What happens to namespace operations not covered by a `sync_dir` (M6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NsPolicy {
    /// Optimistic (an ordered-metadata filesystem; assumption W1): every operation survives.
    KeepAll,
    /// Pessimistic: every non-durable operation is lost.
    DropUnsynced,
    /// Each non-durable operation independently survives with this probability.
    Random(f64),
}

#[derive(Debug, Clone, Copy)]
pub struct CrashCfg {
    pub seed: u64,
    pub data: DataPolicy,
    pub ns: NsPolicy,
    /// Overrides the dump's sector size when non-zero.
    pub sector: usize,
}

impl CrashCfg {
    pub fn strict(seed: u64) -> CrashCfg {
        CrashCfg {
            seed,
            data: DataPolicy::DiscardUnsynced,
            ns: NsPolicy::KeepAll,
            sector: 0,
        }
    }

    pub fn random(seed: u64) -> CrashCfg {
        CrashCfg {
            seed,
            data: DataPolicy::Random {
                reorder: true,
                tear: true,
                length_before_data: true,
            },
            ns: NsPolicy::KeepAll,
            sector: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Dir,
    File(FileId),
}

#[derive(Debug, Clone)]
enum Pending {
    Write { uid: u64, off: u64, data: Vec<u8> },
    SetLen { uid: u64, len: u64 },
}

#[derive(Debug, Default, Clone)]
struct FileData {
    content: Vec<u8>,
    durable: Vec<u8>,
    pending: Vec<Pending>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NsKind {
    Mkdir(String),
    Create { path: String, id: FileId },
    Rename { from: String, to: String },
    Link { from: String, to: String },
    Remove(String),
    Rmdir(String),
}

#[derive(Debug, Clone)]
struct NsOp {
    uid: u64,
    kind: NsKind,
    /// Directories whose `sync_dir` has not yet covered this operation; durable when empty.
    pending_dirs: BTreeSet<String>,
}

impl NsOp {
    fn durable(&self) -> bool {
        self.pending_dirs.is_empty()
    }
}

#[derive(Debug, Default)]
struct State {
    files: HashMap<FileId, FileData>,
    ns: BTreeMap<String, Node>,
    nslog: Vec<NsOp>,
    next_file: FileId,
    uid: u64,
    /// `(shared holders, exclusive held)` per file.
    locks: HashMap<FileId, (u32, bool)>,
    dead: bool,
    lie_counter: u64,
}

/// The simulated filesystem.
#[derive(Debug)]
pub struct SimFs {
    state: Mutex<State>,
    cfg: SimConfig,
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A deterministic 64-bit value for `(seed, tag, a, b)`; independent of the order in which decisions are asked for (M8).
fn h(seed: u64, tag: u64, a: u64, b: u64) -> u64 {
    splitmix64(splitmix64(splitmix64(seed ^ tag.wrapping_mul(0xA24B_AED4_963E_E407)) ^ a) ^ b)
}

fn coin(seed: u64, tag: u64, a: u64, b: u64, per_million: u64) -> bool {
    h(seed, tag, a, b) % 1_000_000 < per_million
}

/// Normalises a path into a rooted, `/`-separated key (verbatim prefixes removed, `.` and `..` resolved lexically).
fn norm(p: &Path) -> String {
    let mut prefix = String::new();
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::Prefix(pc) => {
                let s = pc.as_os_str().to_string_lossy().to_string();
                prefix = s.trim_start_matches(r"\\?\").to_string();
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
        }
    }
    format!("{prefix}/{}", parts.join("/"))
}

fn parent(key: &str) -> Option<String> {
    if key == "/" || key.is_empty() {
        return None;
    }
    let (l, _) = key.rsplit_once('/')?;
    Some(if l.is_empty() || l.ends_with(':') && !l.contains('/') {
        format!("{l}/")
    } else {
        l.to_string()
    })
}

fn err(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

impl State {
    fn bump(&mut self) -> u64 {
        self.uid += 1;
        self.uid
    }

    fn is_dir(&self, key: &str) -> bool {
        key == "/"
            || matches!(self.ns.get(key), Some(Node::Dir))
            || key.ends_with(":/") && self.ns.contains_key(key.trim_end_matches('/'))
    }

    fn log(&mut self, kind: NsKind, dirs: &[Option<String>]) {
        let uid = self.bump();
        let pending_dirs: BTreeSet<String> = dirs.iter().flatten().cloned().collect();
        self.nslog.push(NsOp {
            uid,
            kind,
            pending_dirs,
        });
    }

    fn mkdir_one(&mut self, key: &str) {
        self.ns.insert(key.to_string(), Node::Dir);
        let p = parent(key);
        self.log(NsKind::Mkdir(key.to_string()), &[p]);
    }

    fn descendants(&self, key: &str) -> Vec<String> {
        let prefix = format!("{}/", key.trim_end_matches('/'));
        self.ns
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

impl SimFs {
    /// A new simulated filesystem. `existing_dirs` are directories that exist (and are durable) from the start - the
    /// virtual equivalent of "the temp directory already exists".
    pub fn new(cfg: SimConfig, existing_dirs: &[&Path]) -> Arc<SimFs> {
        let mut st = State {
            next_file: 1,
            ..State::default()
        };
        for d in existing_dirs {
            let key = norm(d);
            // every ancestor, durable
            let mut cur = String::new();
            for part in key.split('/').filter(|s| !s.is_empty()) {
                cur = format!("{cur}/{part}");
                st.ns.entry(cur.clone()).or_insert(Node::Dir);
                let uid = st.bump();
                st.nslog.push(NsOp {
                    uid,
                    kind: NsKind::Mkdir(cur.clone()),
                    pending_dirs: BTreeSet::new(),
                });
            }
        }
        Arc::new(SimFs {
            state: Mutex::new(st),
            cfg,
        })
    }

    pub fn config(&self) -> SimConfig {
        self.cfg
    }

    /// Locks the state; parks forever if the simulated machine has already lost power (so no operation can complete - and be
    /// acknowledged - after the dump instant).
    fn st(&self) -> MutexGuard<'_, State> {
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.dead {
            drop(g);
            loop {
                std::thread::park();
            }
        }
        g
    }

    // ---- namespace ----------------------------------------------------------------------------------------------------

    pub(crate) fn open(self: &Arc<Self>, path: &Path, o: &OpenOptions) -> io::Result<PFile> {
        let key = norm(path);
        let mut st = self.st();
        let writing = o.write || o.append;
        let id = match st.ns.get(&key).cloned() {
            Some(Node::Dir) => return Err(err(io::ErrorKind::PermissionDenied, "is a directory")),
            Some(Node::File(id)) => {
                if o.create_new && writing {
                    return Err(err(io::ErrorKind::AlreadyExists, "file exists"));
                }
                id
            }
            None => {
                if !(o.create || o.create_new) || !writing {
                    return Err(err(io::ErrorKind::NotFound, "no such file"));
                }
                let par = parent(&key).ok_or_else(|| err(io::ErrorKind::NotFound, "no parent"))?;
                if !st.is_dir(&par) {
                    return Err(err(
                        io::ErrorKind::NotFound,
                        "parent directory does not exist",
                    ));
                }
                let id = st.next_file;
                st.next_file += 1;
                st.files.insert(id, FileData::default());
                st.ns.insert(key.clone(), Node::File(id));
                st.log(
                    NsKind::Create {
                        path: key.clone(),
                        id,
                    },
                    &[Some(par)],
                );
                id
            }
        };
        if o.truncate && writing {
            let uid = st.bump();
            let f = st.files.get_mut(&id).expect("file");
            f.content.clear();
            f.pending.push(Pending::SetLen { uid, len: 0 });
        }
        drop(st);
        Ok(PFile::sim(SimFile {
            fs: Arc::clone(self),
            id,
            pos: 0,
            readable: o.read,
            writable: writing,
            append: o.append,
            holds_lock: AtomicU8::new(0),
        }))
    }

    pub(crate) fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        let key = norm(path);
        let st = self.st();
        if !st.is_dir(&key) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        let prefix = format!("{}/", key.trim_end_matches('/'));
        let mut out = Vec::new();
        for (k, node) in st.ns.range(prefix.clone()..) {
            if !k.starts_with(&prefix) {
                break;
            }
            let rest = &k[prefix.len()..];
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            out.push(Ok(DirEntry {
                path: PathBuf::from(k),
                name: OsString::from(rest),
                file_type: FileType {
                    is_dir: *node == Node::Dir,
                    is_file: matches!(node, Node::File(_)),
                },
            }));
        }
        Ok(ReadDir(out.into_iter()))
    }

    pub(crate) fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let key = norm(path);
        let st = self.st();
        match st.ns.get(&key) {
            Some(Node::File(id)) => Ok(Metadata {
                len: st
                    .files
                    .get(id)
                    .map(|f| f.content.len() as u64)
                    .unwrap_or(0),
                is_dir: false,
                is_file: true,
            }),
            Some(Node::Dir) => Ok(Metadata {
                len: 0,
                is_dir: true,
                is_file: false,
            }),
            None if st.is_dir(&key) => Ok(Metadata {
                len: 0,
                is_dir: true,
                is_file: false,
            }),
            None => Err(err(io::ErrorKind::NotFound, "no such file or directory")),
        }
    }

    pub(crate) fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let key = norm(path);
        let st = self.st();
        if st.ns.contains_key(&key) || st.is_dir(&key) {
            Ok(PathBuf::from(key))
        } else {
            Err(err(io::ErrorKind::NotFound, "no such file or directory"))
        }
    }

    pub(crate) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (norm(from), norm(to));
        let mut st = self.st();
        let node = st
            .ns
            .get(&f)
            .cloned()
            .ok_or_else(|| err(io::ErrorKind::NotFound, "rename source missing"))?;
        let tp = parent(&t).ok_or_else(|| err(io::ErrorKind::NotFound, "no parent"))?;
        if !st.is_dir(&tp) {
            return Err(err(
                io::ErrorKind::NotFound,
                "rename target directory missing",
            ));
        }
        match (&node, st.ns.get(&t)) {
            (Node::File(_), Some(Node::Dir)) => {
                return Err(err(
                    io::ErrorKind::PermissionDenied,
                    "target is a directory",
                ))
            }
            (Node::Dir, Some(Node::File(_))) => {
                return Err(err(io::ErrorKind::PermissionDenied, "target is a file"))
            }
            (Node::Dir, Some(Node::Dir)) if !st.descendants(&t).is_empty() => {
                return Err(err(
                    io::ErrorKind::DirectoryNotEmpty,
                    "target directory not empty",
                ))
            }
            _ => {}
        }
        if node == Node::Dir {
            let subtree: Vec<(String, Node)> = st
                .descendants(&f)
                .into_iter()
                .map(|k| {
                    let n = st.ns.get(&k).cloned().expect("node");
                    (k, n)
                })
                .collect();
            for (k, _) in &subtree {
                st.ns.remove(k);
            }
            st.ns.remove(&f);
            st.ns.insert(t.clone(), Node::Dir);
            for (k, n) in subtree {
                st.ns.insert(format!("{t}{}", &k[f.len()..]), n);
            }
        } else {
            st.ns.remove(&f);
            st.ns.insert(t.clone(), node);
        }
        let fp = parent(&f);
        st.log(NsKind::Rename { from: f, to: t }, &[fp, Some(tp)]);
        Ok(())
    }

    pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
        let key = norm(path);
        let mut st = self.st();
        match st.ns.get(&key) {
            Some(Node::File(_)) => {}
            Some(Node::Dir) => return Err(err(io::ErrorKind::PermissionDenied, "is a directory")),
            None => return Err(err(io::ErrorKind::NotFound, "no such file")),
        }
        st.ns.remove(&key);
        let p = parent(&key);
        st.log(NsKind::Remove(key), &[p]);
        Ok(())
    }

    pub(crate) fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let key = norm(path);
        let mut st = self.st();
        if !st.is_dir(&key) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        if !st.descendants(&key).is_empty() {
            return Err(err(io::ErrorKind::DirectoryNotEmpty, "directory not empty"));
        }
        st.ns.remove(&key);
        let p = parent(&key);
        st.log(NsKind::Rmdir(key), &[p]);
        Ok(())
    }

    pub(crate) fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let key = norm(path);
        let mut st = self.st();
        if !st.is_dir(&key) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        let mut subtree = st.descendants(&key);
        subtree.sort_by_key(|k| std::cmp::Reverse(k.matches('/').count()));
        for k in subtree {
            let node = st.ns.remove(&k).expect("node");
            let p = parent(&k);
            st.log(
                if node == Node::Dir {
                    NsKind::Rmdir(k)
                } else {
                    NsKind::Remove(k)
                },
                &[p],
            );
        }
        st.ns.remove(&key);
        let p = parent(&key);
        st.log(NsKind::Rmdir(key), &[p]);
        Ok(())
    }

    pub(crate) fn create_dir(&self, path: &Path, all: bool) -> io::Result<()> {
        let key = norm(path);
        let mut st = self.st();
        if st.is_dir(&key) {
            return if all {
                Ok(())
            } else {
                Err(err(io::ErrorKind::AlreadyExists, "exists"))
            };
        }
        if matches!(st.ns.get(&key), Some(Node::File(_))) {
            return Err(err(
                io::ErrorKind::AlreadyExists,
                "a file exists at that path",
            ));
        }
        if all {
            let mut missing = Vec::new();
            let mut cur = Some(key.clone());
            while let Some(c) = cur {
                if st.is_dir(&c) {
                    break;
                }
                missing.push(c.clone());
                cur = parent(&c);
            }
            for m in missing.into_iter().rev() {
                st.mkdir_one(&m);
            }
        } else {
            let par = parent(&key).ok_or_else(|| err(io::ErrorKind::NotFound, "no parent"))?;
            if !st.is_dir(&par) {
                return Err(err(
                    io::ErrorKind::NotFound,
                    "parent directory does not exist",
                ));
            }
            st.mkdir_one(&key);
        }
        Ok(())
    }

    pub(crate) fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (f, t) = (norm(from), norm(to));
        let mut st = self.st();
        let id = match st.ns.get(&f) {
            Some(Node::File(id)) => *id,
            Some(Node::Dir) => return Err(err(io::ErrorKind::PermissionDenied, "is a directory")),
            None => return Err(err(io::ErrorKind::NotFound, "link source missing")),
        };
        if st.ns.contains_key(&t) {
            return Err(err(io::ErrorKind::AlreadyExists, "link target exists"));
        }
        let tp = parent(&t).ok_or_else(|| err(io::ErrorKind::NotFound, "no parent"))?;
        if !st.is_dir(&tp) {
            return Err(err(io::ErrorKind::NotFound, "target directory missing"));
        }
        st.ns.insert(t.clone(), Node::File(id));
        st.log(NsKind::Link { from: f, to: t }, &[Some(tp)]);
        Ok(())
    }

    /// `fsync` of a directory. With `dir_sync_effective = false` this does nothing (A5: the Windows behaviour).
    pub(crate) fn sync_dir(&self, path: &Path) -> io::Result<()> {
        let key = norm(path);
        let mut st = self.st();
        if !self.cfg.dir_sync_effective {
            return Ok(());
        }
        if !st.is_dir(&key) {
            return Err(err(io::ErrorKind::NotFound, "no such directory"));
        }
        for op in st.nslog.iter_mut() {
            op.pending_dirs.remove(&key);
        }
        Ok(())
    }

    // ---- observation and crash ----------------------------------------------------------------------------------------

    /// Number of live names (files and directories) - for tests.
    pub fn entry_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ns
            .len()
    }

    /// The volatile content of the file at `path` (what a read now would return) - for tests.
    pub fn volatile_content(&self, path: &Path) -> Option<Vec<u8>> {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match st.ns.get(&norm(path)) {
            Some(Node::File(id)) => st.files.get(id).map(|f| f.content.clone()),
            _ => None,
        }
    }

    /// Namespace operations not yet covered by a `sync_dir` (what `NsPolicy::DropUnsynced` would lose), as readable text.
    pub fn non_durable_ns_ops(&self) -> Vec<String> {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.nslog
            .iter()
            .filter(|o| !o.durable())
            .map(|o| format!("{:?} (awaiting sync_dir of {:?})", o.kind, o.pending_dirs))
            .collect()
    }

    /// `(namespace operations awaiting a sync_dir, files holding writes or truncations not yet synced)`. `(0, 0)` means every
    /// crash image of this record is the same: the model has nothing to take away.
    pub fn unsynced_summary(&self) -> (usize, usize) {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (
            st.nslog.iter().filter(|o| !o.durable()).count(),
            st.files.values().filter(|f| !f.pending.is_empty()).count(),
        )
    }

    pub fn list_paths(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ns
            .keys()
            .cloned()
            .collect()
    }

    /// Serialises the whole record and marks the machine dead in one atomic step: afterwards every simulated operation parks,
    /// so nothing can complete (and be acknowledged) after the instant of the dump.
    pub fn dump_and_kill(&self) -> Vec<u8> {
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let bytes = encode(&st, &self.cfg);
        st.dead = true;
        bytes
    }

    /// Serialises without killing (tests).
    pub fn dump(&self) -> Vec<u8> {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        encode(&st, &self.cfg)
    }

    /// Rebuilds a simulator from a dump (to apply a crash to it).
    pub fn from_dump(bytes: &[u8]) -> io::Result<SimFs> {
        let (st, cfg) = decode(bytes)?;
        Ok(SimFs {
            state: Mutex::new(st),
            cfg,
        })
    }

    /// The state after a process kill (crash class B): the OS keeps every byte a returned `write()` handed it and every
    /// namespace operation, so NO part of the power-loss model is applied. This is the "kill-9 cycle skips step 4" image.
    pub fn process_kill_image(&self) -> Survivors {
        self.crash(&CrashCfg {
            seed: 0,
            data: DataPolicy::KeepAll,
            ns: NsPolicy::KeepAll,
            sector: 0,
        })
    }

    /// Applies a simulated power loss to the record and returns what survives. Pure: the same `(record, cfg)` always gives
    /// the same result (M8).
    pub fn crash(&self, c: &CrashCfg) -> Survivors {
        let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let sector = if c.sector != 0 {
            c.sector
        } else {
            self.cfg.sector.max(1)
        };

        // ---- namespace -----------------------------------------------------------------------------------------------
        let mut ns: BTreeMap<String, Node> = BTreeMap::new();
        ns.insert("/".to_string(), Node::Dir);
        let survives = |op: &NsOp| -> bool {
            op.durable()
                || match c.ns {
                    NsPolicy::KeepAll => true,
                    NsPolicy::DropUnsynced => false,
                    NsPolicy::Random(p) => {
                        coin(c.seed, 0x4E53, op.uid, 0, (p * 1_000_000.0) as u64)
                    }
                }
        };
        let is_dir = |ns: &BTreeMap<String, Node>, k: &str| {
            k == "/" || matches!(ns.get(k), Some(Node::Dir)) || k.ends_with(":/")
        };
        for op in &st.nslog {
            if !survives(op) {
                continue;
            }
            match &op.kind {
                NsKind::Mkdir(p) => {
                    if parent(p).is_none_or(|pp| is_dir(&ns, &pp)) {
                        ns.insert(p.clone(), Node::Dir);
                    }
                }
                NsKind::Create { path, id } => {
                    if parent(path).is_none_or(|pp| is_dir(&ns, &pp)) {
                        ns.insert(path.clone(), Node::File(*id));
                    }
                }
                NsKind::Rename { from, to } => {
                    if let Some(node) = ns.get(from).cloned() {
                        if parent(to).is_none_or(|pp| is_dir(&ns, &pp)) {
                            if node == Node::Dir {
                                let prefix = format!("{}/", from.trim_end_matches('/'));
                                let sub: Vec<(String, Node)> = ns
                                    .iter()
                                    .filter(|(k, _)| k.starts_with(&prefix))
                                    .map(|(k, n)| (k.clone(), n.clone()))
                                    .collect();
                                for (k, _) in &sub {
                                    ns.remove(k);
                                }
                                ns.remove(from);
                                ns.insert(to.clone(), Node::Dir);
                                for (k, n) in sub {
                                    ns.insert(format!("{to}{}", &k[from.len()..]), n);
                                }
                            } else {
                                ns.remove(from);
                                ns.insert(to.clone(), node);
                            }
                        }
                    }
                }
                NsKind::Link { from, to } => {
                    if let Some(Node::File(id)) = ns.get(from).cloned() {
                        if parent(to).is_none_or(|pp| is_dir(&ns, &pp)) {
                            ns.insert(to.clone(), Node::File(id));
                        }
                    }
                }
                NsKind::Remove(p) | NsKind::Rmdir(p) => {
                    ns.remove(p);
                }
            }
        }

        // ---- contents ------------------------------------------------------------------------------------------------
        let mut contents: HashMap<FileId, Vec<u8>> = HashMap::new();
        for node in ns.values() {
            if let Node::File(id) = node {
                if contents.contains_key(id) {
                    continue;
                }
                let fd = st.files.get(id);
                let bytes = fd
                    .map(|f| survive_content(f, *id, c, sector))
                    .unwrap_or_default();
                contents.insert(*id, bytes);
            }
        }
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        for (k, node) in &ns {
            match node {
                Node::Dir => {
                    dirs.insert(k.clone());
                }
                Node::File(id) => {
                    files.insert(k.clone(), contents.get(id).cloned().unwrap_or_default());
                }
            }
        }
        Survivors { files, dirs }
    }
}

/// The surviving bytes of one file after a crash (M1, M3, M4, M5).
fn survive_content(f: &FileData, id: FileId, c: &CrashCfg, sector: usize) -> Vec<u8> {
    let mut cur = f.durable.clone();
    let (reorder, tear, length_before_data) = match c.data {
        DataPolicy::DiscardUnsynced => return cur,
        DataPolicy::KeepAll => {
            for op in &f.pending {
                match op {
                    Pending::SetLen { len, .. } => cur.resize(*len as usize, 0),
                    Pending::Write { off, data, .. } => {
                        let end = *off as usize + data.len();
                        if cur.len() < end {
                            cur.resize(end, 0);
                        }
                        cur[*off as usize..end].copy_from_slice(data);
                    }
                }
            }
            return cur;
        }
        DataPolicy::Random {
            reorder,
            tear,
            length_before_data,
        } => (reorder, tear, length_before_data),
    };
    let n = f.pending.len();
    // which pending operations persist
    let mut persist = vec![false; n];
    if reorder {
        for (i, p) in persist.iter_mut().enumerate() {
            *p = coin(c.seed, 0x5052, id, i as u64, 500_000);
        }
    } else {
        let prefix = (h(c.seed, 0x5046, id, 0) % (n as u64 + 1)) as usize;
        for p in persist.iter_mut().take(prefix) {
            *p = true;
        }
    }
    let last_persisted_write = (0..n)
        .rev()
        .find(|&i| persist[i] && matches!(f.pending[i], Pending::Write { .. }));
    for (i, op) in f.pending.iter().enumerate() {
        match op {
            Pending::SetLen { len, .. } => {
                if persist[i] {
                    cur.resize(*len as usize, 0);
                }
            }
            Pending::Write { uid, off, data } => {
                if !persist[i] {
                    if length_before_data && coin(c.seed, 0x4C42, id, *uid, 500_000) {
                        let end = *off as usize + data.len();
                        if cur.len() < end {
                            cur.resize(end, 0);
                        }
                    }
                    continue;
                }
                let torn = tear
                    && last_persisted_write == Some(i)
                    && coin(c.seed, 0x544E, id, *uid, 700_000);
                apply_write(
                    &mut cur,
                    *off as usize,
                    data,
                    sector,
                    torn,
                    c.seed,
                    id,
                    *uid,
                    reorder,
                );
            }
        }
    }
    cur
}

#[allow(clippy::too_many_arguments)]
fn apply_write(
    cur: &mut Vec<u8>,
    off: usize,
    data: &[u8],
    sector: usize,
    torn: bool,
    seed: u64,
    id: u64,
    uid: u64,
    reorder: bool,
) {
    if data.is_empty() {
        return;
    }
    let end = off + data.len();
    let first = off / sector;
    let last = (end - 1) / sector;
    for s in first..=last {
        let lo = (s * sector).max(off);
        let hi = ((s + 1) * sector).min(end);
        let is_last = s == last;
        if torn && is_last {
            // the final sector of the write is torn: first half new, second half new, or none of it
            let mode = h(seed, 0x4D44, id, uid) % 3;
            let mid = lo + (hi - lo) / 2;
            let (a, b) = match mode {
                0 => (lo, mid),
                1 => (mid, hi),
                _ => (lo, lo),
            };
            if b > a {
                if cur.len() < b {
                    cur.resize(b, 0);
                }
                cur[a..b].copy_from_slice(&data[a - off..b - off]);
            }
            continue;
        }
        if reorder && !is_last && !coin(seed, 0x5343, id, uid ^ ((s as u64) << 40), 500_000) {
            continue;
        }
        if cur.len() < hi {
            cur.resize(hi, 0);
        }
        cur[lo..hi].copy_from_slice(&data[lo - off..hi - off]);
    }
}

/// What a simulated power loss leaves behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Survivors {
    pub files: BTreeMap<String, Vec<u8>>,
    pub dirs: BTreeSet<String>,
}

impl Survivors {
    /// Writes the surviving tree under `real_root`, re-rooting the virtual directory `virtual_root` (a path as the engine saw
    /// it) onto `real_root`. Returns the number of files written.
    pub fn write_under(&self, virtual_root: &Path, real_root: &Path) -> io::Result<usize> {
        let vr = norm(virtual_root);
        let vr = vr.trim_end_matches('/').to_string();
        std::fs::create_dir_all(real_root)?;
        let mut n = 0;
        for d in &self.dirs {
            if let Some(rel) = d.strip_prefix(&vr).and_then(|r| r.strip_prefix('/')) {
                std::fs::create_dir_all(real_root.join(rel))?;
            }
        }
        for (k, bytes) in &self.files {
            if let Some(rel) = k.strip_prefix(&vr).and_then(|r| r.strip_prefix('/')) {
                let dest = real_root.join(rel);
                if let Some(p) = dest.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(dest, bytes)?;
                n += 1;
            }
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// handles
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct SimFile {
    fs: Arc<SimFs>,
    id: FileId,
    pos: u64,
    readable: bool,
    writable: bool,
    append: bool,
    /// 0 = holds no lock, 1 = holds a shared lock, 2 = holds the exclusive lock (released on drop; clones never own one).
    holds_lock: AtomicU8,
}

impl SimFile {
    pub(crate) fn sync_all(&self) -> io::Result<()> {
        let mut st = self.fs.st();
        if let Some((seed, per_million)) = self.fs.cfg.fsync_lie {
            st.lie_counter += 1;
            let n = st.lie_counter;
            if coin(seed, 0x4C49, n, 0, per_million as u64) {
                return Ok(()); // M7: the device says "done" and is not
            }
        }
        let f = st
            .files
            .get_mut(&self.id)
            .ok_or_else(|| err(io::ErrorKind::NotFound, "file gone"))?;
        f.durable = f.content.clone();
        f.pending.clear();
        Ok(())
    }

    pub(crate) fn set_len(&self, len: u64) -> io::Result<()> {
        let mut st = self.fs.st();
        let uid = st.bump();
        let f = st
            .files
            .get_mut(&self.id)
            .ok_or_else(|| err(io::ErrorKind::NotFound, "file gone"))?;
        f.content.resize(len as usize, 0);
        f.pending.push(Pending::SetLen { uid, len });
        Ok(())
    }

    pub(crate) fn len(&self) -> io::Result<u64> {
        let st = self.fs.st();
        Ok(st
            .files
            .get(&self.id)
            .map(|f| f.content.len() as u64)
            .unwrap_or(0))
    }

    pub(crate) fn try_clone(&self) -> SimFile {
        SimFile {
            fs: Arc::clone(&self.fs),
            id: self.id,
            pos: self.pos,
            readable: self.readable,
            writable: self.writable,
            append: self.append,
            holds_lock: AtomicU8::new(0),
        }
    }

    pub(crate) fn try_lock(&self, exclusive: bool) -> Result<(), TryLockError> {
        let mut st = self.fs.st();
        let e = st.locks.entry(self.id).or_insert((0, false));
        if exclusive {
            if e.0 > 0 || e.1 {
                return Err(TryLockError::WouldBlock);
            }
            e.1 = true;
            self.holds_lock.store(2, Ordering::SeqCst);
        } else {
            if e.1 {
                return Err(TryLockError::WouldBlock);
            }
            e.0 += 1;
            self.holds_lock.store(1, Ordering::SeqCst);
        }
        Ok(())
    }

    pub(crate) fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.write_at(buf, offset)
    }

    pub(crate) fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let st = self.fs.st();
        let f = st
            .files
            .get(&self.id)
            .ok_or_else(|| err(io::ErrorKind::NotFound, "file gone"))?;
        let end = offset as usize + buf.len();
        if end > f.content.len() {
            return Err(err(
                io::ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            ));
        }
        buf.copy_from_slice(&f.content[offset as usize..end]);
        Ok(())
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        if !self.writable {
            return Err(err(
                io::ErrorKind::PermissionDenied,
                "file not opened for writing",
            ));
        }
        let mut st = self.fs.st();
        let uid = st.bump();
        let f = st
            .files
            .get_mut(&self.id)
            .ok_or_else(|| err(io::ErrorKind::NotFound, "file gone"))?;
        let end = offset as usize + buf.len();
        if f.content.len() < end {
            f.content.resize(end, 0);
        }
        f.content[offset as usize..end].copy_from_slice(buf);
        f.pending.push(Pending::Write {
            uid,
            off: offset,
            data: buf.to_vec(),
        });
        Ok(())
    }
}

impl Drop for SimFile {
    fn drop(&mut self) {
        let held = self.holds_lock.swap(0, Ordering::SeqCst);
        if held != 0 {
            if let Ok(mut st) = self.fs.state.lock() {
                if let Some(e) = st.locks.get_mut(&self.id) {
                    if held == 2 {
                        e.1 = false;
                    } else {
                        e.0 = e.0.saturating_sub(1);
                    }
                }
            }
        }
    }
}

impl Read for SimFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.readable {
            return Err(err(
                io::ErrorKind::PermissionDenied,
                "file not opened for reading",
            ));
        }
        let st = self.fs.st();
        let f = st
            .files
            .get(&self.id)
            .ok_or_else(|| err(io::ErrorKind::NotFound, "file gone"))?;
        let len = f.content.len() as u64;
        if self.pos >= len {
            return Ok(0);
        }
        let n = buf.len().min((len - self.pos) as usize);
        buf[..n].copy_from_slice(&f.content[self.pos as usize..self.pos as usize + n]);
        drop(st);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for SimFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.append {
            self.pos = self.len()?;
        }
        let at = self.pos;
        self.write_at(buf, at)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for SimFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => self.len()? as i128 + d as i128,
        };
        if new < 0 {
            return Err(err(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// dump format (child process -> parent process)
// ---------------------------------------------------------------------------------------------------------------------

const MAGIC: &[u8; 8] = b"RBXSIMD1";

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn encode(st: &State, cfg: &SimConfig) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(MAGIC);
    o.push(cfg.dir_sync_effective as u8);
    put_u64(&mut o, cfg.sector as u64);
    put_u64(&mut o, st.uid);
    put_u64(&mut o, st.files.len() as u64);
    let mut ids: Vec<&FileId> = st.files.keys().collect();
    ids.sort();
    for id in ids {
        let f = &st.files[id];
        put_u64(&mut o, *id);
        put_bytes(&mut o, &f.durable);
        put_u64(&mut o, f.pending.len() as u64);
        for p in &f.pending {
            match p {
                Pending::Write { uid, off, data } => {
                    o.push(0);
                    put_u64(&mut o, *uid);
                    put_u64(&mut o, *off);
                    put_bytes(&mut o, data);
                }
                Pending::SetLen { uid, len } => {
                    o.push(1);
                    put_u64(&mut o, *uid);
                    put_u64(&mut o, *len);
                }
            }
        }
    }
    put_u64(&mut o, st.nslog.len() as u64);
    for op in &st.nslog {
        put_u64(&mut o, op.uid);
        put_u64(&mut o, op.pending_dirs.len() as u64);
        for d in &op.pending_dirs {
            put_bytes(&mut o, d.as_bytes());
        }
        match &op.kind {
            NsKind::Mkdir(p) => {
                o.push(0);
                put_bytes(&mut o, p.as_bytes());
            }
            NsKind::Create { path, id } => {
                o.push(1);
                put_bytes(&mut o, path.as_bytes());
                put_u64(&mut o, *id);
            }
            NsKind::Rename { from, to } => {
                o.push(2);
                put_bytes(&mut o, from.as_bytes());
                put_bytes(&mut o, to.as_bytes());
            }
            NsKind::Link { from, to } => {
                o.push(3);
                put_bytes(&mut o, from.as_bytes());
                put_bytes(&mut o, to.as_bytes());
            }
            NsKind::Remove(p) => {
                o.push(4);
                put_bytes(&mut o, p.as_bytes());
            }
            NsKind::Rmdir(p) => {
                o.push(5);
                put_bytes(&mut o, p.as_bytes());
            }
        }
    }
    o
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn bad<T>(&self) -> io::Result<T> {
        Err(err(io::ErrorKind::InvalidData, "malformed simulator dump"))
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.at + n > self.b.len() {
            return self.bad();
        }
        let s = &self.b[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u64()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?)
            .map_err(|_| err(io::ErrorKind::InvalidData, "non-utf8 path in dump"))
    }
}

fn decode(b: &[u8]) -> io::Result<(State, SimConfig)> {
    let mut r = Reader { b, at: 0 };
    if r.take(8)? != MAGIC {
        return r.bad();
    }
    let cfg = SimConfig {
        dir_sync_effective: r.u8()? != 0,
        sector: r.u64()? as usize,
        fsync_lie: None,
    };
    let mut st = State {
        uid: r.u64()?,
        ..State::default()
    };
    for _ in 0..r.u64()? {
        let id = r.u64()?;
        let durable = r.bytes()?;
        let mut pending = Vec::new();
        for _ in 0..r.u64()? {
            match r.u8()? {
                0 => {
                    let uid = r.u64()?;
                    let off = r.u64()?;
                    let data = r.bytes()?;
                    pending.push(Pending::Write { uid, off, data });
                }
                1 => {
                    let uid = r.u64()?;
                    let len = r.u64()?;
                    pending.push(Pending::SetLen { uid, len });
                }
                _ => return r.bad(),
            }
        }
        st.files.insert(
            id,
            FileData {
                content: Vec::new(),
                durable,
                pending,
            },
        );
    }
    for _ in 0..r.u64()? {
        let uid = r.u64()?;
        let mut pending_dirs = BTreeSet::new();
        for _ in 0..r.u64()? {
            pending_dirs.insert(r.string()?);
        }
        let kind = match r.u8()? {
            0 => NsKind::Mkdir(r.string()?),
            1 => {
                let path = r.string()?;
                NsKind::Create { path, id: r.u64()? }
            }
            2 => {
                let from = r.string()?;
                NsKind::Rename {
                    from,
                    to: r.string()?,
                }
            }
            3 => {
                let from = r.string()?;
                NsKind::Link {
                    from,
                    to: r.string()?,
                }
            }
            4 => NsKind::Remove(r.string()?),
            5 => NsKind::Rmdir(r.string()?),
            _ => return r.bad(),
        };
        st.nslog.push(NsOp {
            uid,
            kind,
            pending_dirs,
        });
    }
    Ok((st, cfg))
}
