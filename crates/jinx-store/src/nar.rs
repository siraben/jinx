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

/// Reused per-file read buffer size (non-unix fallback). A single `read` covers
/// any file up to this size; larger files loop.
#[cfg(not(unix))]
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
    use std::sync::{mpsc, Arc, Condvar, Mutex};

    #[derive(Clone, Copy)]
    enum NodeType {
        Reg,
        Dir,
        Lnk,
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

    // -----------------------------------------------------------------------
    // Two-phase dump: Phase 1 (this/eval thread) walks the tree, runs the path
    // filter in traversal order, and records an ordered node tree plus a job
    // list of regular-file contents to read. Phase 2 emits the NAR tokens in
    // that exact order; a worker pool prefetches file contents on other cores
    // under a byte budget, while the filter and token ordering stay single-
    // threaded. Workers touch only libc + malloc'd buffers, never the VM/GC.
    // -----------------------------------------------------------------------

    /// Files larger than this stream inline on the eval thread in Phase 2
    /// (never enqueued), so a single big file cannot exceed the prefetch budget.
    const INLINE_THRESHOLD: u64 = 16 * 1024 * 1024;
    /// Max bytes of prefetched-but-not-yet-consumed file contents in flight.
    const PREFETCH_BUDGET: u64 = 128 * 1024 * 1024;
    /// Only spin up the worker pool for dumps with enough parallelizable work;
    /// small trees (hello/firefox, single-file `hash_path`) stay fully serial.
    const PAR_MIN_JOBS: usize = 64;
    const PAR_MIN_BYTES: u64 = 8 * 1024 * 1024;

    /// A regular-file content-read job (absolute path + size, from Phase 1).
    struct Job {
        path: CString,
        size: u64,
        /// Streamed inline on the eval thread (size > INLINE_THRESHOLD) rather
        /// than handed to the worker pool.
        inline: bool,
    }

    /// The recorded, order-preserving node tree produced by Phase 1.
    enum RNode {
        File { exec: bool, size: u64, job: usize },
        Symlink { target: Vec<u8> },
        Dir { entries: Vec<(Vec<u8>, RNode)> },
    }

    /// Read a whole regular file (`size` bytes) into a fresh buffer. A short read
    /// (EOF before `size`) means the file shrank while dumping — an error, as in
    /// the serial path. O_NOFOLLOW guards against a swap-to-symlink race.
    fn read_file_whole(path: &CStr, size: u64) -> io::Result<Vec<u8>> {
        let fd = openat_rd(libc::AT_FDCWD, path, 0)?;
        let mut buf = vec![0u8; size as usize];
        let mut off = 0usize;
        while off < buf.len() {
            let n = read_some(fd.as_raw_fd(), &mut buf[off..])?;
            if n == 0 {
                return Err(other_err(format!(
                    "file '{}' changed size while dumping NAR",
                    Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).display()
                )));
            }
            off += n;
        }
        Ok(buf)
    }

    // ---- Phase 1: build the ordered tree + job list -----------------------

    struct Builder<P: PathFilter> {
        filter: P,
        /// Absolute path bytes of the current node (filter arg + error messages).
        path: Vec<u8>,
        jobs: Vec<Job>,
    }

    impl<P: PathFilter> Builder<P> {
        fn build(
            &mut self,
            dirfd: libc::c_int,
            name: &CStr,
            ntype: NodeType,
            depth: usize,
        ) -> io::Result<RNode> {
            if depth >= NAR_MAX_DEPTH {
                return Err(other_err(format!(
                    "path '{}' exceeds maximum NAR directory depth of {}",
                    Path::new(std::ffi::OsStr::from_bytes(&self.path)).display(),
                    NAR_MAX_DEPTH
                )));
            }
            match ntype {
                NodeType::Reg => {
                    // One fstatat gives size + exec; contents are read later.
                    let st = fstatat_nofollow(dirfd, name)?;
                    let size = st.st_size as u64;
                    let exec = st.st_mode as u32 & libc::S_IXUSR as u32 != 0;
                    crate::nar_stats::record_file(size);
                    let cpath = CString::new(self.path.clone())
                        .map_err(|_| other_err("path contains NUL".into()))?;
                    let job = self.jobs.len();
                    self.jobs.push(Job {
                        path: cpath,
                        size,
                        inline: size > INLINE_THRESHOLD,
                    });
                    Ok(RNode::File { exec, size, job })
                }
                NodeType::Lnk => {
                    crate::nar_stats::record_symlink();
                    let target = readlinkat_bytes(dirfd, name, 0)?;
                    Ok(RNode::Symlink { target })
                }
                NodeType::Dir => {
                    crate::nar_stats::record_dir();
                    let dfd = openat_rd(dirfd, name, libc::O_DIRECTORY)?;
                    let listing = read_dir_entries(dfd.as_raw_fd())?;
                    let mut entries: Vec<(Vec<u8>, RNode)> = Vec::with_capacity(listing.len());
                    let base = self.path.len();
                    for (cname, d_type) in &listing {
                        self.path.push(b'/');
                        self.path.extend_from_slice(cname);

                        let keep = if P::ACTIVE {
                            let p = Path::new(std::ffi::OsStr::from_bytes(&self.path));
                            self.filter.keep(p)?
                        } else {
                            true
                        };
                        if keep {
                            let ccname = CString::new(cname.as_slice()).map_err(|_| {
                                other_err("directory entry name contains NUL".into())
                            })?;
                            let ct = match *d_type {
                                libc::DT_DIR => NodeType::Dir,
                                libc::DT_REG => NodeType::Reg,
                                libc::DT_LNK => NodeType::Lnk,
                                _ => {
                                    let st = fstatat_nofollow(dfd.as_raw_fd(), &ccname)?;
                                    mode_to_type(st.st_mode as u32, &self.path)?
                                }
                            };
                            let child = self.build(dfd.as_raw_fd(), &ccname, ct, depth + 1)?;
                            entries.push((cname.clone(), child));
                        }
                        self.path.truncate(base);
                    }
                    Ok(RNode::Dir { entries })
                }
            }
        }
    }

    // ---- Phase 2: emit tokens, pulling file contents from a provider ------

    /// Supplies a regular file's contents (by job index) during emission.
    trait ContentProvider {
        fn get(&mut self, job: usize, size: u64) -> io::Result<Vec<u8>>;
    }

    fn emit_node<W: Write, Pv: ContentProvider>(
        node: &RNode,
        sink: &mut W,
        pv: &mut Pv,
    ) -> io::Result<()> {
        wire::write_bytes(sink, b"(")?;
        match node {
            RNode::File { exec, size, job } => {
                wire::write_bytes(sink, b"type")?;
                wire::write_bytes(sink, b"regular")?;
                if *exec {
                    wire::write_bytes(sink, b"executable")?;
                    wire::write_bytes(sink, b"")?;
                }
                let contents = pv.get(*job, *size)?;
                wire::write_bytes(sink, b"contents")?;
                wire::write_u64(sink, *size)?;
                sink.write_all(&contents)?;
                wire::write_padding(sink, *size)?;
            }
            RNode::Symlink { target } => {
                wire::write_bytes(sink, b"type")?;
                wire::write_bytes(sink, b"symlink")?;
                wire::write_bytes(sink, b"target")?;
                wire::write_bytes(sink, target)?;
            }
            RNode::Dir { entries } => {
                wire::write_bytes(sink, b"type")?;
                wire::write_bytes(sink, b"directory")?;
                for (name, child) in entries {
                    wire::write_bytes(sink, b"entry")?;
                    wire::write_bytes(sink, b"(")?;
                    wire::write_bytes(sink, b"name")?;
                    wire::write_bytes(sink, name)?;
                    wire::write_bytes(sink, b"node")?;
                    emit_node(child, sink, pv)?;
                    wire::write_bytes(sink, b")")?;
                }
            }
        }
        wire::write_bytes(sink, b")")
    }

    /// Serial provider: read each file inline on the eval thread (no pool).
    struct SerialProvider {
        jobs: Vec<Job>,
    }
    impl ContentProvider for SerialProvider {
        fn get(&mut self, job: usize, size: u64) -> io::Result<Vec<u8>> {
            read_file_whole(&self.jobs[job].path, size)
        }
    }

    /// Shared state between the eval thread and the read-ahead workers.
    struct Shared {
        jobs: Vec<Job>,
        /// One slot per job; a worker fills its slot then notifies `cv`.
        slots: Mutex<Vec<Option<io::Result<Vec<u8>>>>>,
        cv: Condvar,
    }
    impl Shared {
        fn take(&self, idx: usize) -> io::Result<Vec<u8>> {
            let mut g = self.slots.lock().unwrap();
            while g[idx].is_none() {
                g = self.cv.wait(g).unwrap();
            }
            g[idx].take().unwrap()
        }
    }

    fn worker_loop(shared: Arc<Shared>, rx: Arc<Mutex<mpsc::Receiver<usize>>>) {
        loop {
            let idx = {
                let rx = rx.lock().unwrap();
                rx.recv()
            };
            let idx = match idx {
                Ok(i) => i,
                Err(_) => break, // sender dropped: no more work
            };
            let job = &shared.jobs[idx];
            let res = read_file_whole(&job.path, job.size);
            let mut g = shared.slots.lock().unwrap();
            g[idx] = Some(res);
            shared.cv.notify_all();
        }
    }

    /// Parallel provider: a bounded read-ahead window over the worker pool.
    /// Jobs are submitted in index (= emission) order; `in_flight` tracks the
    /// bytes of submitted-but-not-yet-consumed prefetch jobs, capped at the
    /// budget. Inline jobs are never submitted (read directly in `get`).
    struct ParallelProvider {
        shared: Arc<Shared>,
        tx: Option<mpsc::Sender<usize>>,
        workers: Vec<std::thread::JoinHandle<()>>,
        next_submit: usize,
        in_flight: u64,
    }

    impl ParallelProvider {
        fn submit_one(&mut self) {
            let job = &self.shared.jobs[self.next_submit];
            if !job.inline {
                self.in_flight += job.size;
                // Sender lives until Drop, so unwrap is safe here.
                let _ = self.tx.as_ref().unwrap().send(self.next_submit);
            }
            self.next_submit += 1;
        }
        /// Ensure every job up to and including `target` has been submitted.
        fn submit_through(&mut self, target: usize) {
            while self.next_submit <= target {
                self.submit_one();
            }
        }
        /// Read ahead while there is budget headroom (always keep >=1 in flight).
        fn submit_readahead(&mut self) {
            let njobs = self.shared.jobs.len();
            while self.next_submit < njobs {
                let job = &self.shared.jobs[self.next_submit];
                if !job.inline
                    && self.in_flight != 0
                    && self.in_flight + job.size > PREFETCH_BUDGET
                {
                    break;
                }
                self.submit_one();
            }
        }
    }

    impl ContentProvider for ParallelProvider {
        fn get(&mut self, job: usize, size: u64) -> io::Result<Vec<u8>> {
            if self.shared.jobs[job].inline {
                // Big file: stream it on the eval thread, off the budget.
                return read_file_whole(&self.shared.jobs[job].path, size);
            }
            self.submit_through(job);
            self.submit_readahead();
            let res = self.shared.take(job);
            self.in_flight -= size;
            self.submit_readahead();
            res
        }
    }

    impl Drop for ParallelProvider {
        fn drop(&mut self) {
            // Close the channel so idle workers exit, then join them. Workers
            // never block on `cv` (only the eval thread waits), so this cannot
            // deadlock.
            self.tx = None;
            for w in self.workers.drain(..) {
                let _ = w.join();
            }
        }
    }

    /// Number of read-ahead workers, from `JINX_NAR_JOBS`. The parallel
    /// prefetch pool is **opt-in**: measured on both aarch64-darwin and
    /// x86_64-linux it trades 2-3x the system CPU for only ~8% ISO wall on a
    /// warm page cache (page-cache lock contention on parallel reads), which is
    /// a poor default for an interactively-run tool. It is a real win for
    /// cold-cache / CI / batch dumps, where workers block on IO instead of
    /// spinning on locks -- so it stays available behind the env var.
    ///
    /// Unset or `<= 1` => 0 (serial, no pool). `auto` => `available_parallelism`
    /// capped at 8. A number `>= 2` => that many workers, capped at 16.
    fn num_workers() -> usize {
        match std::env::var("JINX_NAR_JOBS") {
            Ok(v) if v == "auto" => std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(8),
            Ok(v) => v
                .parse::<usize>()
                .ok()
                .filter(|&n| n >= 2)
                .map(|n| n.min(16))
                .unwrap_or(0),
            Err(_) => 0,
        }
    }

    pub(super) fn dump_root<W: Write, P: PathFilter>(
        path: &Path,
        sink: &mut W,
        filter: P,
    ) -> io::Result<()> {
        let path_bytes = path.as_os_str().as_bytes().to_vec();
        let cpath =
            CString::new(path_bytes.clone()).map_err(|_| other_err("path contains NUL".into()))?;
        // The root type is unknown up front; one `fstatat` (no follow) resolves it.
        let st = fstatat_nofollow(libc::AT_FDCWD, &cpath)?;
        let ntype = mode_to_type(st.st_mode as u32, &path_bytes)?;

        // Phase 1: walk + filter (this thread only), recording the node tree.
        let mut builder = Builder {
            filter,
            path: path_bytes,
            jobs: Vec::new(),
        };
        let root = builder.build(libc::AT_FDCWD, &cpath, ntype, 0)?;
        let jobs = builder.jobs;

        // Phase 2: emit in order. Use the worker pool only when it is opted in
        // (`JINX_NAR_JOBS`, see `num_workers`) AND there is enough
        // parallelizable content to amortize thread spin-up. Default is serial.
        let workers_n = num_workers();
        let prefetch_bytes: u64 = jobs.iter().filter(|j| !j.inline).map(|j| j.size).sum();
        let prefetch_count = jobs.iter().filter(|j| !j.inline).count();

        if workers_n >= 2 && prefetch_count >= PAR_MIN_JOBS && prefetch_bytes >= PAR_MIN_BYTES {
            let njobs = jobs.len();
            let shared = Arc::new(Shared {
                jobs,
                slots: Mutex::new((0..njobs).map(|_| None).collect()),
                cv: Condvar::new(),
            });
            let (tx, rx) = mpsc::channel::<usize>();
            let rx = Arc::new(Mutex::new(rx));
            let mut workers = Vec::new();
            for _ in 0..workers_n {
                let s = Arc::clone(&shared);
                let r = Arc::clone(&rx);
                workers.push(std::thread::spawn(move || worker_loop(s, r)));
            }
            let mut pv = ParallelProvider {
                shared,
                tx: Some(tx),
                workers,
                next_submit: 0,
                in_flight: 0,
            };
            pv.submit_readahead();
            emit_node(&root, sink, &mut pv)
            // `pv` drops here: closes the channel and joins the workers.
        } else {
            let mut pv = SerialProvider { jobs };
            emit_node(&root, sink, &mut pv)
        }
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
