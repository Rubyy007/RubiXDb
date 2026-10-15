//! The persistence layer (ADR-PERSISTENCE-TRAIT-01): every durable byte and every namespace change of the engine and of
//! `ops` goes through this module.
//!
//! **`RealFs`** is what a product build contains: each function forwards to the `std::fs` call of the same name, and
//! [`PFile`] is a newtype around `std::fs::File`. There is no dynamic dispatch, no global state and no behavioural change -
//! the Windows no-op `sync_dir` and the Windows `seek_write` loop are the ones the engine always had.
//!
//! **`SimFs`** (`sim`, only with the cargo feature `sim-fs`, never in a product build) records every write and every
//! namespace operation together with what was made durable, and can apply a simulated power loss to that record
//! (`docs/PERSISTENCE_MODEL.md`, assumptions M1-M8). When a simulator is installed in the process every function here routes
//! to it instead of `std::fs`.
//!
//! `tests/persistence_abstraction_complete.rs` fails if production code outside this module touches `std::fs` directly.

use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub use std::fs::TryLockError;

#[cfg(feature = "sim-fs")]
pub mod sim;

#[cfg(feature = "sim-fs")]
#[inline]
fn sim_active() -> Option<std::sync::Arc<sim::SimFs>> {
    sim::active()
}

// ---------------------------------------------------------------------------------------------------------------------
// handles
// ---------------------------------------------------------------------------------------------------------------------

/// A file handle. Product builds: `std::fs::File`. Simulation builds: either that or a simulated handle.
#[derive(Debug)]
pub struct PFile {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Real(std::fs::File),
    #[cfg(feature = "sim-fs")]
    Sim(sim::SimFile),
}

impl PFile {
    fn real(f: std::fs::File) -> PFile {
        PFile {
            inner: Inner::Real(f),
        }
    }

    #[cfg(feature = "sim-fs")]
    pub(crate) fn sim(f: sim::SimFile) -> PFile {
        PFile {
            inner: Inner::Sim(f),
        }
    }

    /// `File::open` (read only).
    pub fn open(path: impl AsRef<Path>) -> io::Result<PFile> {
        open(path)
    }

    /// `File::create` (create or truncate, write only).
    pub fn create(path: impl AsRef<Path>) -> io::Result<PFile> {
        create(path)
    }

    /// Positional exact read that does not touch the cursor (the SSTable reader's `read_exact_at`).
    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        match &self.inner {
            Inner::Real(f) => real_read_exact_at(f, buf, offset),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.read_exact_at(buf, offset),
        }
    }

    pub fn sync_all(&self) -> io::Result<()> {
        match &self.inner {
            Inner::Real(f) => f.sync_all(),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.sync_all(),
        }
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        match &self.inner {
            Inner::Real(f) => f.set_len(len),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.set_len(len),
        }
    }

    /// `std::fs::File::metadata` (only `len`, `is_file`, `is_dir` are offered).
    pub fn metadata(&self) -> io::Result<Metadata> {
        match &self.inner {
            Inner::Real(f) => f.metadata().map(Metadata::from_std),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.len().map(|len| Metadata {
                len,
                is_dir: false,
                is_file: true,
            }),
        }
    }

    pub fn try_clone(&self) -> io::Result<PFile> {
        match &self.inner {
            Inner::Real(f) => f.try_clone().map(PFile::real),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => Ok(PFile::sim(f.try_clone())),
        }
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        match &self.inner {
            Inner::Real(f) => f.try_lock(),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.try_lock(true),
        }
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        match &self.inner {
            Inner::Real(f) => f.try_lock_shared(),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.try_lock(false),
        }
    }

    /// Positional write that does not depend on the cursor (the WAL's `write_all_at`).
    pub fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        match &mut self.inner {
            Inner::Real(f) => crate::wal::WalFile::write_all_at(f, buf, offset),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.write_all_at(buf, offset),
        }
    }
}

impl Read for PFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Real(f) => f.read(buf),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.read(buf),
        }
    }
}

impl Write for PFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Real(f) => f.write(buf),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Real(f) => f.flush(),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.flush(),
        }
    }
}

impl Seek for PFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match &mut self.inner {
            Inner::Real(f) => f.seek(pos),
            #[cfg(feature = "sim-fs")]
            Inner::Sim(f) => f.seek(pos),
        }
    }
}

#[cfg(unix)]
fn real_read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(windows)]
fn real_read_exact_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut read = 0usize;
    while read < buf.len() {
        match FileExt::seek_read(file, &mut buf[read..], offset + read as u64) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "read_exact_at: seek_read returned 0 before buffer was full",
                ))
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The WAL's file abstraction over a raw `std::fs::File` - the `RealFs` adapter, moved here from `wal::file_io` so that the
/// only production `std::fs` use in the engine lives in this module.
impl crate::wal::WalFile for std::fs::File {
    fn sync_all(&self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        std::fs::File::set_len(self, len)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    #[cfg(unix)]
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        FileExt::write_all_at(self, buf, offset)
    }

    #[cfg(windows)]
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        // `seek_write` is a positional write that does not move the file's own cursor on Unix but DOES leave the position at
        // the end of the written region on Windows (verified when this was written); nothing relies on the position. Windows'
        // `FileExt` has no `write_all_at`, so loop to match Unix's contract, including the `Interrupted` retry rule.
        let mut written = 0usize;
        while written < buf.len() {
            match FileExt::seek_write(self, &buf[written..], offset + written as u64) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write_all_at: underlying seek_write returned 0",
                    ))
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl crate::wal::WalFile for PFile {
    fn sync_all(&self) -> io::Result<()> {
        PFile::sync_all(self)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        PFile::set_len(self, len)
    }

    fn size(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> io::Result<()> {
        PFile::write_all_at(self, buf, offset)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// open options
// ---------------------------------------------------------------------------------------------------------------------

/// `std::fs::OpenOptions`, same builder.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) append: bool,
    pub(crate) create: bool,
    pub(crate) create_new: bool,
    pub(crate) truncate: bool,
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions::default()
    }

    pub fn read(&mut self, v: bool) -> &mut Self {
        self.read = v;
        self
    }

    pub fn write(&mut self, v: bool) -> &mut Self {
        self.write = v;
        self
    }

    pub fn append(&mut self, v: bool) -> &mut Self {
        self.append = v;
        self
    }

    pub fn create(&mut self, v: bool) -> &mut Self {
        self.create = v;
        self
    }

    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.create_new = v;
        self
    }

    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.truncate = v;
        self
    }

    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<PFile> {
        #[cfg(feature = "sim-fs")]
        if let Some(s) = sim_active() {
            return s.open(path.as_ref(), self);
        }
        std::fs::OpenOptions::new()
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .create(self.create)
            .create_new(self.create_new)
            .truncate(self.truncate)
            .open(path)
            .map(PFile::real)
    }
}

/// `File::open`.
pub fn open(path: impl AsRef<Path>) -> io::Result<PFile> {
    OpenOptions::new().read(true).open(path)
}

/// `File::create`.
pub fn create(path: impl AsRef<Path>) -> io::Result<PFile> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

// ---------------------------------------------------------------------------------------------------------------------
// metadata and directory listing
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub(crate) len: u64,
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
}

impl Metadata {
    fn from_std(m: std::fs::Metadata) -> Metadata {
        Metadata {
            len: m.len(),
            is_dir: m.is_dir(),
            is_file: m.is_file(),
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        self.is_file
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileType {
    pub(crate) is_dir: bool,
    pub(crate) is_file: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        self.is_file
    }
}

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub(crate) path: PathBuf,
    pub(crate) name: OsString,
    pub(crate) file_type: FileType,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.file_type)
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata(&self.path)
    }
}

/// The entries of one directory, listed eagerly (an engine directory is small; a listing is a snapshot either way).
pub struct ReadDir(std::vec::IntoIter<io::Result<DirEntry>>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.read_dir(path.as_ref());
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(path)? {
        out.push(e.and_then(|e| {
            let ft = e.file_type()?;
            Ok(DirEntry {
                path: e.path(),
                name: e.file_name(),
                file_type: FileType {
                    is_dir: ft.is_dir(),
                    is_file: ft.is_file(),
                },
            })
        }));
    }
    Ok(ReadDir(out.into_iter()))
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.metadata(path.as_ref());
    }
    std::fs::metadata(path).map(Metadata::from_std)
}

/// `Path::exists`.
pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// `Path::is_dir`.
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// `Path::is_file`.
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.canonicalize(path.as_ref());
    }
    std::fs::canonicalize(path)
}

// ---------------------------------------------------------------------------------------------------------------------
// namespace operations
// ---------------------------------------------------------------------------------------------------------------------

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.rename(from.as_ref(), to.as_ref());
    }
    std::fs::rename(from, to)
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.remove_file(path.as_ref());
    }
    std::fs::remove_file(path)
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.remove_dir(path.as_ref());
    }
    std::fs::remove_dir(path)
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.remove_dir_all(path.as_ref());
    }
    std::fs::remove_dir_all(path)
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.create_dir(path.as_ref(), false);
    }
    std::fs::create_dir(path)
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.create_dir(path.as_ref(), true);
    }
    std::fs::create_dir_all(path)
}

/// `create_dir_all`, then makes every directory entry this call created durable by syncing the PARENT of each
/// (ADR-DATA-DIR-DURABILITY-01). On Windows `sync_dir` is a no-op (assumption A5/W1), so there this is exactly
/// `create_dir_all`; a directory the call did not create is not touched.
pub fn create_dir_all_durable(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let mut created: Vec<PathBuf> = Vec::new();
    for a in path.ancestors() {
        if a.as_os_str().is_empty() || exists(a) {
            break;
        }
        created.push(a.to_path_buf());
    }
    create_dir_all(path)?;
    let mut parents: Vec<PathBuf> = Vec::new();
    for d in &created {
        let parent = match d.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    for parent in parents {
        sync_dir(&parent)?;
    }
    Ok(())
}

pub fn hard_link(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if let Some(s) = sim_active() {
        return s.hard_link(from.as_ref(), to.as_ref());
    }
    std::fs::hard_link(from, to)
}

/// `std::fs::copy` (content only; returns the byte count).
pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    #[cfg(feature = "sim-fs")]
    if sim_active().is_some() {
        let bytes = read(from)?;
        write(to, &bytes)?;
        return Ok(bytes.len() as u64);
    }
    std::fs::copy(from, to)
}

/// `std::fs::read`.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    #[cfg(feature = "sim-fs")]
    if sim_active().is_some() {
        let mut f = open(path)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        return Ok(v);
    }
    std::fs::read(path)
}

/// `std::fs::read_to_string`.
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    #[cfg(feature = "sim-fs")]
    if sim_active().is_some() {
        let bytes = read(path)?;
        return String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        });
    }
    std::fs::read_to_string(path)
}

/// `std::fs::write` (create or truncate, write everything; no sync).
pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    #[cfg(feature = "sim-fs")]
    if sim_active().is_some() {
        let mut f = create(path)?;
        return f.write_all(contents.as_ref());
    }
    std::fs::write(path, contents)
}

/// The simulator hook of the directory-sync primitive. `wal::fsync_dir` (which keeps the platform behaviour and the test
/// hook) calls this first.
#[cfg(feature = "sim-fs")]
pub(crate) fn sim_sync_dir(path: &Path) -> Option<io::Result<()>> {
    sim_active().map(|s| s.sync_dir(path))
}

/// Makes the namespace operations on `path` (a directory) durable where the platform can: `RealFs` on Unix `fsync`s the
/// directory, on Windows it does nothing (A5); a simulator does what its configuration says.
pub fn sync_dir(path: impl AsRef<Path>) -> io::Result<()> {
    crate::wal::fsync_dir(path.as_ref())
}
