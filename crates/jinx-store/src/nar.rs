//! NAR (Nix ARchive) serialization.
//!
//! Port of the dump side of `src/libutil/archive.cc`, byte-exact, with the
//! macOS "case hack" OFF (Nix's canonical NAR format).

use std::io::{self, Write};
use std::path::Path;

use crate::hash::{Hash, HashAlgorithm, HashSink};
use crate::wire;

/// `nix-archive-1`, the version magic at the start of every NAR.
pub const NAR_VERSION_MAGIC_1: &[u8] = b"nix-archive-1";

/// Maximum directory nesting depth (matches `narMaxDepth`).
const NAR_MAX_DEPTH: usize = 64;

/// Reused per-file read buffer size. A single `read` covers any file up to this
/// size; larger files loop. 256 KiB balances syscall count against cache reuse.
const READ_BUF_SIZE: usize = 256 * 1024;

fn other_err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// Serialize the file-system object at `path` as a NAR into `sink`.
///
/// Port of `SourceAccessor::dumpPath` (over the real file system, no path
/// filter, case hack off). Regular files, directories (entries sorted by
/// byte-wise name order) and symlinks are supported.
pub fn dump_path(path: impl AsRef<Path>, sink: &mut impl Write) -> io::Result<()> {
    crate::nar_stats::record_dump_call();
    wire::write_bytes(sink, NAR_VERSION_MAGIC_1)?;
    dump_root(path.as_ref(), sink, NoFilter)
}

/// Serialize the file-system object at `path` as a NAR into `sink`, applying
/// `filter` to each directory entry (root always included).
///
/// Port of `SourceAccessor::dumpPath` with a `PathFilter` (`archive.cc`): for
/// every directory child, `filter` is called with the child's path; entries for
/// which it returns `false` are omitted. The `filter` may return an error
/// (propagated), mirroring C++ where the filter callback can throw (the
/// evaluator's path-filter function raising an eval error).
pub fn dump_path_filtered<F>(
    path: impl AsRef<Path>,
    sink: &mut impl Write,
    filter: &mut F,
) -> io::Result<()>
where
    F: FnMut(&Path) -> io::Result<bool>,
{
    crate::nar_stats::record_dump_call();
    wire::write_bytes(sink, NAR_VERSION_MAGIC_1)?;
    // `&mut F` is itself `FnMut`, so it satisfies `PathFilter` directly.
    dump_root(path.as_ref(), sink, filter)
}

/// A directory-entry filter. `NoFilter` includes everything (the unfiltered
/// dump), while a real closure mirrors `PathFilter` and may raise.
trait PathFilter {
    /// Whether the child at `path` should be included. `NoFilter` never
    /// allocates a path (the unfiltered dump does no per-entry work here).
    fn keep(&mut self, path: &Path) -> io::Result<bool>;
    /// Whether this is a real filter (controls whether the walk must
    /// materialize child paths to pass to `keep`).
    const ACTIVE: bool;
}

struct NoFilter;
impl PathFilter for NoFilter {
    #[inline]
    fn keep(&mut self, _path: &Path) -> io::Result<bool> {
        Ok(true)
    }
    const ACTIVE: bool = false;
}

impl<F: FnMut(&Path) -> io::Result<bool>> PathFilter for &mut F {
    #[inline]
    fn keep(&mut self, path: &Path) -> io::Result<bool> {
        (self)(path)
    }
    const ACTIVE: bool = true;
}

// ---------------------------------------------------------------------------
// Unix: dirfd-relative libc walk (openat/fstat/fstatat/readlinkat + d_type).
// ---------------------------------------------------------------------------
#[cfg(unix)]
mod imp {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    #[derive(Clone, Copy)]
    enum NodeType {
        Reg,
        Dir,
        Lnk,
    }

    /// State threaded through the recursive walk. `path` holds the absolute path
    /// of the current node as raw bytes (used for the filter callback and error
    /// messages); `buf` is the reused file read buffer.
    struct Walk<'a, W: Write, P: PathFilter> {
        sink: &'a mut W,
        filter: P,
        path: Vec<u8>,
        buf: Vec<u8>,
    }

    fn last_err() -> io::Error {
        io::Error::last_os_error()
    }

    #[cfg(target_os = "linux")]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__errno_location()
    }
    #[cfg(not(target_os = "linux"))]
    unsafe fn errno_location() -> *mut libc::c_int {
        libc::__error()
    }

    /// `openat` with O_RDONLY|O_CLOEXEC|O_NOFOLLOW (plus `extra`). O_NOFOLLOW is
    /// safe here because we only `open` nodes whose `d_type`/mode says regular
    /// file or directory — never a symlink — so it merely hardens against a
    /// concurrent swap-to-symlink race.
    fn openat_rd(dirfd: libc::c_int, name: &CStr, extra: libc::c_int) -> io::Result<OwnedFd> {
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | extra;
        // SAFETY: `name` is a valid NUL-terminated C string; `dirfd` is a live
        // directory fd or AT_FDCWD. On success `openat` returns a fresh fd we
        // take sole ownership of via `OwnedFd`.
        let fd = unsafe { libc::openat(dirfd, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(last_err());
        }
        // SAFETY: `fd >= 0` is a freshly-opened descriptor owned by no one else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn fstat(fd: libc::c_int) -> io::Result<libc::stat> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fd` is a live descriptor; `st` is a valid, writable buffer
        // that `fstat` fully initializes on success.
        let r = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
        if r < 0 {
            return Err(last_err());
        }
        // SAFETY: `fstat` returned 0, so `st` is initialized.
        Ok(unsafe { st.assume_init() })
    }

    fn fstatat_nofollow(dirfd: libc::c_int, name: &CStr) -> io::Result<libc::stat> {
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `dirfd`/`name` are valid; `st` is a valid writable buffer that
        // `fstatat` fully initializes on success.
        let r = unsafe {
            libc::fstatat(
                dirfd,
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r < 0 {
            return Err(last_err());
        }
        // SAFETY: `fstatat` returned 0, so `st` is initialized.
        Ok(unsafe { st.assume_init() })
    }

    fn readlinkat_bytes(dirfd: libc::c_int, name: &CStr, size_hint: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; size_hint.clamp(64, 4096)];
        loop {
            // SAFETY: `dirfd`/`name` are valid; `buf` is a live allocation of
            // `buf.len()` bytes that `readlinkat` writes into (never NUL-terminated).
            let n = unsafe {
                libc::readlinkat(
                    dirfd,
                    name.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    buf.len(),
                )
            };
            if n < 0 {
                return Err(last_err());
            }
            let n = n as usize;
            if n < buf.len() {
                buf.truncate(n);
                return Ok(buf);
            }
            // Filled the buffer: target may have been truncated, grow and retry.
            buf.resize(buf.len() * 2, 0);
        }
    }

    /// One `read` (retrying on EINTR). Returns 0 at EOF.
    fn read_some(fd: libc::c_int, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: `fd` is a live descriptor; `buf` is a valid mutable slice
            // of `buf.len()` bytes.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let e = last_err();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }

    fn mode_to_type(mode: u32, path: &[u8]) -> io::Result<NodeType> {
        match mode & (libc::S_IFMT as u32) {
            m if m == libc::S_IFREG as u32 => Ok(NodeType::Reg),
            m if m == libc::S_IFDIR as u32 => Ok(NodeType::Dir),
            m if m == libc::S_IFLNK as u32 => Ok(NodeType::Lnk),
            _ => Err(other_err(format!(
                "file '{}' has an unsupported type",
                Path::new(std::ffi::OsStr::from_bytes(path)).display()
            ))),
        }
    }

    /// Enumerate a directory's entries (excluding `.`/`..`) into a byte-wise
    /// name-sorted map of name -> `d_type`. `d_type` may be `DT_UNKNOWN` on some
    /// filesystems, resolved later via `fstatat`.
    fn read_dir_entries(dirfd: libc::c_int) -> io::Result<BTreeMap<Vec<u8>, u8>> {
        // `fdopendir` takes ownership of the fd it is given (closedir closes it),
        // so hand it a dup and keep `dirfd` alive for the subsequent `openat`s.
        // SAFETY: `dirfd` is a live directory descriptor.
        let dup = unsafe { libc::dup(dirfd) };
        if dup < 0 {
            return Err(last_err());
        }
        // SAFETY: `dup` is a fresh live descriptor; `fdopendir` adopts it.
        let dirp = unsafe { libc::fdopendir(dup) };
        if dirp.is_null() {
            let e = last_err();
            // SAFETY: `dup` is still live (fdopendir failed to adopt it).
            unsafe { libc::close(dup) };
            return Err(e);
        }
        let mut map = BTreeMap::new();
        let result = (|| loop {
            // Reset errno so a NULL return can be classified as end-of-dir
            // (errno unchanged) vs. error (errno set).
            // SAFETY: `errno_location` returns a valid pointer to this thread's errno.
            unsafe { *errno_location() = 0 };
            // SAFETY: `dirp` is a live DIR* returned by fdopendir.
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                // SAFETY: reading this thread's errno.
                let err = unsafe { *errno_location() };
                if err != 0 {
                    return Err(io::Error::from_raw_os_error(err));
                }
                return Ok(());
            }
            // SAFETY: `ent` is a valid dirent; `d_name` is a NUL-terminated
            // array within it, valid until the next readdir/closedir.
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            // SAFETY: `ent` is valid; `d_type` is a plain byte field.
            let d_type = unsafe { (*ent).d_type };
            map.insert(name.to_vec(), d_type);
        })();
        // SAFETY: `dirp` is a live DIR*; closedir consumes it (and closes `dup`).
        unsafe { libc::closedir(dirp) };
        result.map(|()| map)
    }

    impl<W: Write, P: PathFilter> Walk<'_, W, P> {
        /// Dump one node whose type is already known.
        fn dump_node(
            &mut self,
            dirfd: libc::c_int,
            name: &CStr,
            ntype: NodeType,
            depth: usize,
        ) -> io::Result<()> {
            if depth >= NAR_MAX_DEPTH {
                return Err(other_err(format!(
                    "path '{}' exceeds maximum NAR directory depth of {}",
                    Path::new(std::ffi::OsStr::from_bytes(&self.path)).display(),
                    NAR_MAX_DEPTH
                )));
            }

            wire::write_bytes(self.sink, b"(")?;
            match ntype {
                NodeType::Reg => {
                    let fd = openat_rd(dirfd, name, 0)?;
                    let st = fstat(fd.as_raw_fd())?;
                    wire::write_bytes(self.sink, b"type")?;
                    wire::write_bytes(self.sink, b"regular")?;
                    if st.st_mode as u32 & libc::S_IXUSR as u32 != 0 {
                        wire::write_bytes(self.sink, b"executable")?;
                        wire::write_bytes(self.sink, b"")?;
                    }
                    self.dump_contents(fd.as_raw_fd(), st.st_size as u64)?;
                }
                NodeType::Dir => {
                    crate::nar_stats::record_dir();
                    wire::write_bytes(self.sink, b"type")?;
                    wire::write_bytes(self.sink, b"directory")?;
                    let dfd = openat_rd(dirfd, name, libc::O_DIRECTORY)?;
                    self.dump_dir(dfd.as_raw_fd(), depth)?;
                }
                NodeType::Lnk => {
                    crate::nar_stats::record_symlink();
                    wire::write_bytes(self.sink, b"type")?;
                    wire::write_bytes(self.sink, b"symlink")?;
                    wire::write_bytes(self.sink, b"target")?;
                    let target = readlinkat_bytes(dirfd, name, 0)?;
                    wire::write_bytes(self.sink, &target)?;
                }
            }
            wire::write_bytes(self.sink, b")")
        }

        /// Emit a regular file's `contents`: tag, u64 size, exactly `size` bytes
        /// read from `fd`, then zero padding to 8 bytes. A short read (EOF before
        /// `size`) means the file shrank while dumping — preserved as an error.
        fn dump_contents(&mut self, fd: libc::c_int, size: u64) -> io::Result<()> {
            crate::nar_stats::record_file(size);
            wire::write_bytes(self.sink, b"contents")?;
            wire::write_u64(self.sink, size)?;
            let mut remaining = size;
            while remaining > 0 {
                let want = remaining.min(self.buf.len() as u64) as usize;
                let n = read_some(fd, &mut self.buf[..want])?;
                if n == 0 {
                    return Err(other_err(format!(
                        "file '{}' changed size while dumping NAR",
                        Path::new(std::ffi::OsStr::from_bytes(&self.path)).display()
                    )));
                }
                self.sink.write_all(&self.buf[..n])?;
                remaining -= n as u64;
            }
            wire::write_padding(self.sink, size)
        }

        /// Walk the entries of the directory `dirfd`, in byte-wise name order,
        /// applying the filter to each child (in order; first error wins).
        fn dump_dir(&mut self, dirfd: libc::c_int, depth: usize) -> io::Result<()> {
            let entries = read_dir_entries(dirfd)?;
            let base = self.path.len();
            for (name, d_type) in &entries {
                // Extend the current path with "/name" for the filter/errors.
                self.path.push(b'/');
                self.path.extend_from_slice(name);

                let keep = if P::ACTIVE {
                    let p = Path::new(std::ffi::OsStr::from_bytes(&self.path));
                    self.filter.keep(p)?
                } else {
                    true
                };

                if keep {
                    // Names never contain NUL, so `CString::new` cannot fail.
                    let cname = CString::new(name.as_slice())
                        .map_err(|_| other_err("directory entry name contains NUL".into()))?;
                    let ntype = match *d_type {
                        libc::DT_DIR => NodeType::Dir,
                        libc::DT_REG => NodeType::Reg,
                        libc::DT_LNK => NodeType::Lnk,
                        // DT_UNKNOWN and any exotic type: resolve with fstatat.
                        _ => {
                            let st = fstatat_nofollow(dirfd, &cname)?;
                            mode_to_type(st.st_mode as u32, &self.path)?
                        }
                    };
                    wire::write_bytes(self.sink, b"entry")?;
                    wire::write_bytes(self.sink, b"(")?;
                    wire::write_bytes(self.sink, b"name")?;
                    wire::write_bytes(self.sink, name)?;
                    wire::write_bytes(self.sink, b"node")?;
                    self.dump_node(dirfd, &cname, ntype, depth + 1)?;
                    wire::write_bytes(self.sink, b")")?;
                }

                self.path.truncate(base);
            }
            Ok(())
        }
    }

    pub(super) fn dump_root<W: Write, P: PathFilter>(
        path: &Path,
        sink: &mut W,
        filter: P,
    ) -> io::Result<()> {
        let path_bytes = path.as_os_str().as_bytes().to_vec();
        let cpath = CString::new(path_bytes.clone())
            .map_err(|_| other_err("path contains NUL".into()))?;
        // The root type is unknown up front; one `fstatat` (no follow) resolves it.
        let st = fstatat_nofollow(libc::AT_FDCWD, &cpath)?;
        let ntype = mode_to_type(st.st_mode as u32, &path_bytes)?;
        let mut walk = Walk {
            sink,
            filter,
            path: path_bytes,
            buf: vec![0u8; READ_BUF_SIZE],
        };
        walk.dump_node(libc::AT_FDCWD, &cpath, ntype, 0)
    }
}

#[cfg(unix)]
use imp::dump_root;

// ---------------------------------------------------------------------------
// Non-unix fallback: portable std recursion (no dirfd / d_type available).
// ---------------------------------------------------------------------------
#[cfg(not(unix))]
fn dump_root<W: Write, P: PathFilter>(path: &Path, sink: &mut W, mut filter: P) -> io::Result<()> {
    dump_std(path, sink, &mut filter, 0)
}

#[cfg(not(unix))]
fn dump_std<W: Write, P: PathFilter>(
    path: &Path,
    sink: &mut W,
    filter: &mut P,
    depth: usize,
) -> io::Result<()> {
    use std::collections::BTreeMap;

    if depth >= NAR_MAX_DEPTH {
        return Err(other_err(format!(
            "path '{}' exceeds maximum NAR directory depth of {}",
            path.display(),
            NAR_MAX_DEPTH
        )));
    }

    let st = std::fs::symlink_metadata(path)?;
    let ft = st.file_type();

    wire::write_bytes(sink, b"(")?;
    if ft.is_file() {
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"regular")?;
        crate::nar_stats::record_file(st.len());
        wire::write_bytes(sink, b"contents")?;
        wire::write_u64(sink, st.len())?;
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; READ_BUF_SIZE];
        let mut remaining = st.len();
        use std::io::Read;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = file.read(&mut buf[..want])?;
            if n == 0 {
                return Err(other_err(format!(
                    "file '{}' changed size while dumping NAR",
                    path.display()
                )));
            }
            sink.write_all(&buf[..n])?;
            remaining -= n as u64;
        }
        wire::write_padding(sink, st.len())?;
    } else if ft.is_dir() {
        crate::nar_stats::record_dir();
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"directory")?;
        let mut entries: BTreeMap<Vec<u8>, std::path::PathBuf> = BTreeMap::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned().into_bytes();
            entries.insert(name, entry.path());
        }
        for (name, entry_path) in &entries {
            if P::ACTIVE && !filter.keep(entry_path)? {
                continue;
            }
            wire::write_bytes(sink, b"entry")?;
            wire::write_bytes(sink, b"(")?;
            wire::write_bytes(sink, b"name")?;
            wire::write_bytes(sink, name)?;
            wire::write_bytes(sink, b"node")?;
            dump_std(entry_path, sink, filter, depth + 1)?;
            wire::write_bytes(sink, b")")?;
        }
    } else if ft.is_symlink() {
        crate::nar_stats::record_symlink();
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"symlink")?;
        wire::write_bytes(sink, b"target")?;
        let target = std::fs::read_link(path)?;
        let target_bytes = target.to_string_lossy().into_owned().into_bytes();
        wire::write_bytes(sink, &target_bytes)?;
    } else {
        return Err(other_err(format!(
            "file '{}' has an unsupported type",
            path.display()
        )));
    }
    wire::write_bytes(sink, b")")
}

/// Serialize a string as a single-regular-file NAR.
///
/// Port of `dumpString`.
pub fn dump_string(s: &[u8], sink: &mut impl Write) -> io::Result<()> {
    for tok in [NAR_VERSION_MAGIC_1, b"(", b"type", b"regular", b"contents"] {
        wire::write_bytes(sink, tok)?;
    }
    wire::write_bytes(sink, s)?;
    wire::write_bytes(sink, b")")
}

/// Dump `path` and return the whole NAR as bytes.
pub fn dump_path_to_vec(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    dump_path(path, &mut out)?;
    Ok(out)
}

/// Hash the NAR serialization of `path` without materializing it.
/// Returns the hash and the NAR size in bytes.
pub fn hash_path(path: impl AsRef<Path>, algo: HashAlgorithm) -> io::Result<(Hash, u64)> {
    let mut sink = HashSink::new(algo);
    dump_path(path, &mut sink)?;
    let (hash, sz) = sink.finish();
    crate::nar_stats::record_nar_bytes(sz);
    Ok((hash, sz))
}
