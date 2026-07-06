//! Nix daemon worker-protocol client.
//!
//! Port of the client side of `src/libstore/{worker-protocol.cc,
//! worker-protocol-connection.cc,remote-store.cc,uds-remote-store.cc}`.
//!
//! This connects (blocking) to a local `nix-daemon` over a Unix socket, runs
//! the magic/version/feature handshake, then exposes typed operations. Only the
//! negotiated protocol version `>= 1.35` is supported (older daemons are
//! rejected). Daemon stderr messages are logged to this process's stderr, as
//! C++ Nix does; remote errors are decoded into [`DaemonError::Remote`].
//!
//! # Example
//!
//! ```no_run
//! use jinx_store::daemon::DaemonStore;
//! let mut store = DaemonStore::connect().unwrap();
//! println!("daemon version: {:?}", store.daemon_nix_version());
//! ```

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::hash::{Hash, HashAlgorithm};
use crate::store_path::{ContentAddress, ContentAddressMethod, StoreDir, StorePath, StorePathSet};
use crate::{nar, wire};

// ---------------------------------------------------------------------------
// Protocol constants (worker-protocol.hh)
// ---------------------------------------------------------------------------

/// `WORKER_MAGIC_1`, sent by the client to open a connection.
pub const WORKER_MAGIC_1: u64 = 0x6e69_7863;
/// `WORKER_MAGIC_2`, echoed by the daemon.
pub const WORKER_MAGIC_2: u64 = 0x6478_696f;

/// The protocol version this client advertises (1.38).
pub const CLIENT_VERSION: u16 = (1 << 8) | 38;
/// The oldest negotiated version we will speak (1.35).
pub const MIN_SUPPORTED_VERSION: u16 = (1 << 8) | 35;

/// `GET_PROTOCOL_MAJOR`.
pub const fn proto_major(v: u16) -> u16 {
    v & 0xff00
}
/// `GET_PROTOCOL_MINOR`.
pub const fn proto_minor(v: u16) -> u16 {
    v & 0x00ff
}

// STDERR message tags.
const STDERR_NEXT: u64 = 0x6f6c_6d67;
const STDERR_READ: u64 = 0x6461_7461;
const STDERR_WRITE: u64 = 0x6461_7416;
const STDERR_LAST: u64 = 0x616c_7473;
const STDERR_ERROR: u64 = 0x6378_7470;
const STDERR_START_ACTIVITY: u64 = 0x5354_5254;
const STDERR_STOP_ACTIVITY: u64 = 0x5354_4f50;
const STDERR_RESULT: u64 = 0x5253_4c54;

/// Feature names this client advertises (only exchanged when the negotiated
/// version is `>= 1.38`).
const CLIENT_FEATURES: &[&str] = &["realisation-with-path-not-hash", "delete-dead-specific-referrers"];

/// Generous cap for daemon-sent strings (store paths, hashes, sigs are small;
/// log lines can be larger).
const MAX_STRING: u64 = 256 << 20;

/// Worker-protocol operation codes (`WorkerProto::Op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Op {
    IsValidPath = 1,
    QueryReferrers = 6,
    AddToStore = 7,
    BuildPaths = 9,
    EnsurePath = 10,
    AddTempRoot = 11,
    AddIndirectRoot = 12,
    SyncWithGC = 13,
    FindRoots = 14,
    SetOptions = 19,
    CollectGarbage = 20,
    QueryAllValidPaths = 23,
    QueryPathInfo = 26,
    QueryPathFromHashPart = 29,
    QueryValidPaths = 31,
    QuerySubstitutablePaths = 32,
    QueryValidDerivers = 33,
    OptimiseStore = 34,
    VerifyStore = 35,
    BuildDerivation = 36,
    AddSignatures = 37,
    NarFromPath = 38,
    AddToStoreNar = 39,
    QueryMissing = 40,
    QueryDerivationOutputMap = 41,
    RegisterDrvOutput = 42,
    QueryRealisation = 43,
    AddMultipleToStore = 44,
    AddBuildLog = 45,
    BuildPathsWithResults = 46,
    AddPermRoot = 47,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An error from the daemon client.
#[derive(Debug)]
pub enum DaemonError {
    /// A low-level I/O or connection error.
    Io(io::Error),
    /// A protocol-level violation (bad magic, unexpected message, etc.).
    Protocol(String),
    /// A structured error forwarded by the daemon (from a `STDERR_ERROR`).
    Remote {
        /// Verbosity level of the error.
        level: u64,
        /// The error message.
        msg: String,
        /// Trace hints attached to the error.
        traces: Vec<String>,
    },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Io(e) => write!(f, "daemon I/O error: {e}"),
            DaemonError::Protocol(s) => write!(f, "daemon protocol error: {s}"),
            DaemonError::Remote { msg, .. } => write!(f, "daemon error: {msg}"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<io::Error> for DaemonError {
    fn from(e: io::Error) -> Self {
        DaemonError::Io(e)
    }
}

type Result<T> = std::result::Result<T, DaemonError>;

fn protocol_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(DaemonError::Protocol(msg.into()))
}

// ---------------------------------------------------------------------------
// Typed wire values
// ---------------------------------------------------------------------------

/// Metadata about a valid store path.
///
/// Port of `UnkeyedValidPathInfo` + the keyed `path` (`ValidPathInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidPathInfo {
    pub path: StorePath,
    pub deriver: Option<StorePath>,
    pub nar_hash: Hash,
    pub references: StorePathSet,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: BTreeSet<String>,
    pub ca: Option<ContentAddress>,
}

/// Which outputs of a derivation a [`DerivedPath::Built`] refers to.
///
/// Port of `OutputsSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputsSpec {
    /// All outputs (`*`).
    All,
    /// A specific, non-empty set of output names.
    Names(BTreeSet<String>),
}

impl OutputsSpec {
    /// Port of `OutputsSpec::to_string`.
    pub fn render(&self) -> String {
        match self {
            OutputsSpec::All => "*".to_string(),
            OutputsSpec::Names(names) => names.iter().cloned().collect::<Vec<_>>().join(","),
        }
    }
}

/// A build request target.
///
/// Port of `DerivedPath` (single-level: we don't model nested dynamic
/// derivations, which is sufficient for the evaluator's needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedPath {
    /// A plain store path (`DerivedPath::Opaque`).
    Opaque(StorePath),
    /// Build the given outputs of a `.drv` (`DerivedPath::Built`).
    Built { drv_path: StorePath, outputs: OutputsSpec },
}

impl DerivedPath {
    /// Port of `DerivedPath::to_string_legacy` (version `>= 1.30` form): an
    /// opaque path renders as its store path, a built path as
    /// `<drv>!<outputs>`.
    pub fn to_string_legacy(&self, store: &StoreDir) -> String {
        match self {
            DerivedPath::Opaque(p) => store.print_store_path(p),
            DerivedPath::Built { drv_path, outputs } => {
                format!("{}!{}", store.print_store_path(drv_path), outputs.render())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Socket path resolution
// ---------------------------------------------------------------------------

/// The default daemon socket path.
pub const DEFAULT_SOCKET_PATH: &str = "/nix/var/nix/daemon-socket/socket";

/// Resolve the daemon socket path.
///
/// Honors `NIX_DAEMON_SOCKET_PATH`, then `NIX_REMOTE` (`daemon`,
/// `unix://<path>` / `unix:<path>`), falling back to [`DEFAULT_SOCKET_PATH`].
pub fn resolve_socket_path() -> String {
    if let Ok(p) = std::env::var("NIX_DAEMON_SOCKET_PATH") {
        if !p.is_empty() {
            return p;
        }
    }
    if let Ok(remote) = std::env::var("NIX_REMOTE") {
        if let Some(rest) = remote.strip_prefix("unix://") {
            if !rest.is_empty() {
                return rest.to_string();
            }
        } else if let Some(rest) = remote.strip_prefix("unix:") {
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
        // "daemon", "unix", or empty -> default socket.
    }
    DEFAULT_SOCKET_PATH.to_string()
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// A blocking worker-protocol client over a Unix-domain socket.
pub struct DaemonStore {
    stream: UnixStream,
    version: u16,
    features: BTreeSet<String>,
    daemon_nix_version: Option<String>,
    trusted: Option<bool>,
    store_dir: StoreDir,
}

impl DaemonStore {
    /// Connect to the daemon at the resolved socket path (see
    /// [`resolve_socket_path`]) and complete the handshake.
    pub fn connect() -> Result<Self> {
        Self::connect_path(&resolve_socket_path())
    }

    /// Connect to the daemon at a specific socket path.
    pub fn connect_path(path: impl AsRef<Path>) -> Result<Self> {
        let stream = UnixStream::connect(path)?;
        let mut store = DaemonStore {
            stream,
            version: 0,
            features: BTreeSet::new(),
            daemon_nix_version: None,
            trusted: None,
            store_dir: StoreDir::default(),
        };
        store.handshake()?;
        Ok(store)
    }

    /// The negotiated protocol version (`major << 8 | minor`).
    pub fn version(&self) -> u16 {
        self.version
    }

    /// The negotiated feature set.
    pub fn features(&self) -> &BTreeSet<String> {
        &self.features
    }

    /// The daemon's reported Nix version (protocol `>= 1.33`).
    pub fn daemon_nix_version(&self) -> Option<&str> {
        self.daemon_nix_version.as_deref()
    }

    /// Whether the daemon trusts this client (protocol `>= 1.35`); `None` if
    /// unknown.
    pub fn trusted(&self) -> Option<bool> {
        self.trusted
    }

    /// The store directory this client parses/prints paths against.
    pub fn store_dir(&self) -> &StoreDir {
        &self.store_dir
    }

    fn at_least(&self, major: u16, minor: u16) -> bool {
        self.version >= ((major << 8) | minor)
    }

    // -- handshake --------------------------------------------------------

    fn handshake(&mut self) -> Result<()> {
        let mut w = &self.stream;
        wire::write_u64(&mut w, WORKER_MAGIC_1)?;
        wire::write_u64(&mut w, CLIENT_VERSION as u64)?;
        w.flush()?;

        let mut r = &self.stream;
        let magic = wire::read_u64(&mut r)?;
        if magic != WORKER_MAGIC_2 {
            return protocol_err(format!(
                "nix-daemon protocol mismatch: got magic 0x{magic:x}"
            ));
        }
        let daemon_version = wire::read_u64(&mut r)? as u16;
        if proto_major(daemon_version) != (1 << 8) {
            return protocol_err("Nix daemon protocol major version not supported");
        }
        self.version = daemon_version.min(CLIENT_VERSION);

        // Feature exchange (negotiated version >= 1.38).
        if self.at_least(1, 38) {
            let feats: Vec<&str> = CLIENT_FEATURES.to_vec();
            let mut w = &self.stream;
            wire::write_bytes_list(&mut w, &feats)?;
            w.flush()?;
            let mut r = &self.stream;
            let daemon_feats = wire::read_bytes_list(&mut r, MAX_STRING)?;
            let daemon_feats: BTreeSet<String> = daemon_feats
                .into_iter()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .collect();
            self.features = CLIENT_FEATURES
                .iter()
                .map(|s| s.to_string())
                .filter(|f| daemon_feats.contains(f))
                .collect();
        }

        if self.version < MIN_SUPPORTED_VERSION {
            return protocol_err(format!(
                "the Nix daemon protocol version {}.{} is too old (need >= 1.35)",
                proto_major(self.version) >> 8,
                proto_minor(self.version)
            ));
        }

        // postHandshake: obsolete fields then ClientHandshakeInfo.
        let mut w = &self.stream;
        if self.at_least(1, 14) {
            wire::write_u64(&mut w, 0)?; // obsolete cpu affinity (0 = none)
        }
        if self.at_least(1, 11) {
            wire::write_u64(&mut w, 0)?; // obsolete reserveSpace = false
        }
        if self.at_least(1, 33) {
            w.flush()?;
        }
        let mut r = &self.stream;
        if self.at_least(1, 33) {
            let v = wire::read_bytes(&mut r, MAX_STRING)?;
            self.daemon_nix_version = Some(String::from_utf8_lossy(&v).into_owned());
        }
        if self.at_least(1, 35) {
            // optional<TrustedFlag>: `readNum<uint8_t>` reads a full 8-byte LE
            // integer (0=unknown, 1=trusted, 2=not-trusted) validated to fit a
            // byte — it is NOT a single wire byte.
            let tag = wire::read_u64(&mut r)?;
            self.trusted = match tag {
                0 => None,
                1 => Some(true),
                2 => Some(false),
                other => return protocol_err(format!("invalid trust value {other}")),
            };
        }

        // Drain the daemon's post-handshake stderr.
        self.process_stderr()?;

        // SetOptions (we never advertise disable-set-options).
        self.set_options()?;

        Ok(())
    }

    fn set_options(&mut self) -> Result<()> {
        let mut w = &self.stream;
        wire::write_u64(&mut w, Op::SetOptions as u64)?;
        wire::write_u64(&mut w, 0)?; // keepFailed
        wire::write_u64(&mut w, 0)?; // keepGoing
        wire::write_u64(&mut w, 0)?; // tryFallback
        wire::write_u64(&mut w, 0)?; // verbosity (lvlError, minimal stderr)
        wire::write_u64(&mut w, 1)?; // maxBuildJobs
        wire::write_u64(&mut w, 0)?; // maxSilentTime
        wire::write_u64(&mut w, 1)?; // obsolete useBuildHook = true
        wire::write_u64(&mut w, 7)?; // buildVerbosity (lvlVomit)
        wire::write_u64(&mut w, 0)?; // obsolete log type
        wire::write_u64(&mut w, 0)?; // obsolete print build trace
        wire::write_u64(&mut w, 0)?; // buildCores
        wire::write_u64(&mut w, 1)?; // useSubstitutes = true
        wire::write_u64(&mut w, 0)?; // overrides: empty map
        w.flush()?;
        self.process_stderr()
    }

    // -- stderr loop ------------------------------------------------------

    /// Port of `processStderr` (blocking): consume daemon log/activity
    /// messages up to `STDERR_LAST`, forwarding text to this process's stderr.
    /// Returns `Err(DaemonError::Remote)` on `STDERR_ERROR`.
    fn process_stderr(&self) -> Result<()> {
        let mut r = &self.stream;
        loop {
            let msg = wire::read_u64(&mut r)?;
            match msg {
                STDERR_WRITE => {
                    let _ = wire::read_bytes(&mut r, MAX_STRING)?;
                }
                STDERR_READ => {
                    // The daemon wants input from our (absent) source. We only
                    // stream via FramedSink, so this is unexpected.
                    let _len = wire::read_u64(&mut r)?;
                    return protocol_err("daemon requested input via STDERR_READ");
                }
                STDERR_ERROR => {
                    return Err(self.read_error()?);
                }
                STDERR_NEXT => {
                    let line = wire::read_bytes(&mut r, MAX_STRING)?;
                    eprint!("{}", String::from_utf8_lossy(&line));
                }
                STDERR_START_ACTIVITY => {
                    let _act = wire::read_u64(&mut r)?;
                    let _lvl = wire::read_u64(&mut r)?;
                    let _type = wire::read_u64(&mut r)?;
                    let _s = wire::read_bytes(&mut r, MAX_STRING)?;
                    self.read_fields()?;
                    let _parent = wire::read_u64(&mut r)?;
                }
                STDERR_STOP_ACTIVITY => {
                    let _act = wire::read_u64(&mut r)?;
                }
                STDERR_RESULT => {
                    let _act = wire::read_u64(&mut r)?;
                    let _type = wire::read_u64(&mut r)?;
                    self.read_fields()?;
                }
                STDERR_LAST => return Ok(()),
                other => return protocol_err(format!("unknown stderr message type 0x{other:x}")),
            }
        }
    }

    /// Port of `readFields`.
    fn read_fields(&self) -> Result<()> {
        let mut r = &self.stream;
        let size = wire::read_u64(&mut r)?;
        for _ in 0..size {
            let ty = wire::read_u64(&mut r)?;
            match ty {
                0 => {
                    let _ = wire::read_u64(&mut r)?;
                }
                1 => {
                    let _ = wire::read_bytes(&mut r, MAX_STRING)?;
                }
                other => return protocol_err(format!("unknown field type {other}")),
            }
        }
        Ok(())
    }

    /// Port of `readError` (protocol `>= 1.26`).
    fn read_error(&self) -> Result<DaemonError> {
        let mut r = &self.stream;
        let type_ = wire::read_bytes(&mut r, MAX_STRING)?;
        if type_ != b"Error" {
            return protocol_err("STDERR_ERROR with non-'Error' type");
        }
        let level = wire::read_u64(&mut r)?;
        let _name = wire::read_bytes(&mut r, MAX_STRING)?; // removed field
        let msg = wire::read_bytes(&mut r, MAX_STRING)?;
        let have_pos = wire::read_u64(&mut r)?;
        if have_pos != 0 {
            return protocol_err("error positions are not supported");
        }
        let ntraces = wire::read_u64(&mut r)?;
        let mut traces = Vec::with_capacity(ntraces.min(1024) as usize);
        for _ in 0..ntraces {
            let hp = wire::read_u64(&mut r)?;
            if hp != 0 {
                return protocol_err("trace positions are not supported");
            }
            let t = wire::read_bytes(&mut r, MAX_STRING)?;
            traces.push(String::from_utf8_lossy(&t).into_owned());
        }
        Ok(DaemonError::Remote {
            level,
            msg: String::from_utf8_lossy(&msg).into_owned(),
            traces,
        })
    }

    // -- wire helpers -----------------------------------------------------

    fn write_op(&self, op: Op) -> Result<()> {
        let mut w = &self.stream;
        wire::write_u64(&mut w, op as u64)?;
        Ok(())
    }

    fn write_store_path(&self, path: &StorePath) -> Result<()> {
        let mut w = &self.stream;
        wire::write_bytes(&mut w, self.store_dir.print_store_path(path).as_bytes())?;
        Ok(())
    }

    fn write_store_path_set(&self, set: &StorePathSet) -> Result<()> {
        let mut w = &self.stream;
        wire::write_u64(&mut w, set.len() as u64)?;
        for p in set {
            wire::write_bytes(&mut w, self.store_dir.print_store_path(p).as_bytes())?;
        }
        Ok(())
    }

    fn read_store_path(&self) -> Result<StorePath> {
        let mut r = &self.stream;
        let s = wire::read_bytes(&mut r, MAX_STRING)?;
        let s = String::from_utf8_lossy(&s);
        self.store_dir
            .parse_store_path(&s)
            .map_err(|e| DaemonError::Protocol(format!("bad store path from daemon: {e}")))
    }

    fn read_store_path_set(&self) -> Result<StorePathSet> {
        let mut r = &self.stream;
        let count = wire::read_u64(&mut r)?;
        let mut set = StorePathSet::new();
        for _ in 0..count {
            set.insert(self.read_store_path()?);
        }
        Ok(set)
    }

    fn read_bool(&self) -> Result<bool> {
        let mut r = &self.stream;
        Ok(wire::read_u64(&mut r)? != 0)
    }

    fn flush(&self) -> Result<()> {
        (&self.stream).flush()?;
        Ok(())
    }

    // -- operations -------------------------------------------------------

    /// `IsValidPath` (op 1).
    pub fn is_valid_path(&mut self, path: &StorePath) -> Result<bool> {
        self.write_op(Op::IsValidPath)?;
        self.write_store_path(path)?;
        self.flush()?;
        self.process_stderr()?;
        self.read_bool()
    }

    /// `QueryValidPaths` (op 31). `substitute` requests the daemon also
    /// consider substitutable paths (protocol `>= 1.27`).
    pub fn query_valid_paths(
        &mut self,
        paths: &StorePathSet,
        substitute: bool,
    ) -> Result<StorePathSet> {
        self.write_op(Op::QueryValidPaths)?;
        self.write_store_path_set(paths)?;
        if self.at_least(1, 27) {
            let mut w = &self.stream;
            wire::write_u64(&mut w, substitute as u64)?;
        }
        self.flush()?;
        self.process_stderr()?;
        self.read_store_path_set()
    }

    /// `QueryPathInfo` (op 26). Returns `None` if the path is not valid.
    pub fn query_path_info(&mut self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
        self.write_op(Op::QueryPathInfo)?;
        self.write_store_path(path)?;
        self.flush()?;
        self.process_stderr()?;
        // Protocol >= 1.17: leading "valid" bool.
        if self.at_least(1, 17) {
            if !self.read_bool()? {
                return Ok(None);
            }
        }
        let info = self.read_unkeyed_valid_path_info(path.clone())?;
        Ok(Some(info))
    }

    fn read_unkeyed_valid_path_info(&self, path: StorePath) -> Result<ValidPathInfo> {
        let mut r = &self.stream;
        let deriver_s = wire::read_bytes(&mut r, MAX_STRING)?;
        let deriver = if deriver_s.is_empty() {
            None
        } else {
            let s = String::from_utf8_lossy(&deriver_s);
            Some(
                self.store_dir
                    .parse_store_path(&s)
                    .map_err(|e| DaemonError::Protocol(format!("bad deriver: {e}")))?,
            )
        };
        let nar_hash_s = wire::read_bytes(&mut r, MAX_STRING)?;
        let nar_hash = Hash::parse_non_sri_unprefixed(
            &String::from_utf8_lossy(&nar_hash_s),
            HashAlgorithm::Sha256,
        )
        .map_err(|e| DaemonError::Protocol(format!("bad narHash: {e}")))?;
        let references = self.read_store_path_set()?;
        let mut r = &self.stream;
        let registration_time = wire::read_u64(&mut r)?;
        let nar_size = wire::read_u64(&mut r)?;

        let mut ultimate = false;
        let mut sigs = BTreeSet::new();
        let mut ca = None;
        if self.at_least(1, 16) {
            ultimate = self.read_bool()?;
            let mut r = &self.stream;
            let nsigs = wire::read_u64(&mut r)?;
            for _ in 0..nsigs {
                let s = wire::read_bytes(&mut r, MAX_STRING)?;
                sigs.insert(String::from_utf8_lossy(&s).into_owned());
            }
            let mut r2 = &self.stream;
            let ca_s = wire::read_bytes(&mut r2, MAX_STRING)?;
            if !ca_s.is_empty() {
                ca = Some(
                    ContentAddress::parse(&String::from_utf8_lossy(&ca_s))
                        .map_err(|e| DaemonError::Protocol(format!("bad ca: {e}")))?,
                );
            }
        }

        Ok(ValidPathInfo {
            path,
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        })
    }

    /// `AddTempRoot` (op 11): prevent `path` from being garbage-collected for
    /// the lifetime of this connection.
    pub fn add_temp_root(&mut self, path: &StorePath) -> Result<()> {
        self.write_op(Op::AddTempRoot)?;
        self.write_store_path(path)?;
        self.flush()?;
        self.process_stderr()?;
        let _ = self.read_bool()?; // ignored result word
        Ok(())
    }

    /// `EnsurePath` (op 10): realise/substitute `path` so it becomes valid.
    pub fn ensure_path(&mut self, path: &StorePath) -> Result<()> {
        self.write_op(Op::EnsurePath)?;
        self.write_store_path(path)?;
        self.flush()?;
        self.process_stderr()?;
        let _ = self.read_bool()?;
        Ok(())
    }

    /// `AddToStore` (op 7, protocol `>= 1.25`): add the NAR serialization of
    /// the tree at `real_path` under `name`, content-addressed by
    /// `method`/`algo`. Streams the NAR via a [`FramedSink`].
    ///
    /// Returns the resulting store path (from the daemon's `ValidPathInfo`).
    pub fn add_to_store_nar_path(
        &mut self,
        name: &str,
        real_path: &Path,
        method: ContentAddressMethod,
        algo: HashAlgorithm,
        references: &StorePathSet,
        repair: bool,
    ) -> Result<ValidPathInfo> {
        if !self.at_least(1, 25) {
            return protocol_err("AddToStore requires protocol >= 1.25");
        }
        self.write_op(Op::AddToStore)?;
        let mut w = &self.stream;
        wire::write_bytes(&mut w, name.as_bytes())?;
        wire::write_bytes(&mut w, cam_str(method, algo).as_bytes())?;
        self.write_store_path_set(references)?;
        let mut w = &self.stream;
        wire::write_u64(&mut w, repair as u64)?;
        w.flush()?;

        // Stream the NAR as framed chunks.
        {
            let mut framed = FramedSink::new(&self.stream);
            nar::dump_path(real_path, &mut framed)?;
            framed.finish()?;
        }
        self.flush()?;

        self.process_stderr()?;
        // Response: ValidPathInfo (keyed path + unkeyed info).
        let path = self.read_store_path()?;
        self.read_unkeyed_valid_path_info(path)
    }

    /// Serialize a `BuildPaths` (op 9) request. Exposed for testing; not run
    /// against the live daemon here.
    pub fn build_paths(&mut self, paths: &[DerivedPath], build_mode: u64) -> Result<()> {
        let mut buf = Vec::new();
        write_derived_paths(&mut buf, &self.store_dir, paths)?;
        wire::write_u64(&mut buf, build_mode)?;

        self.write_op(Op::BuildPaths)?;
        let mut w = &self.stream;
        w.write_all(&buf)?;
        w.flush()?;
        self.process_stderr()?;
        let _ = self.read_bool()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AddToStore helpers
// ---------------------------------------------------------------------------

/// Port of `ContentAddressMethod::renderWithAlgo`: e.g. `fixed:r:sha256`
/// (recursive NAR), `fixed:sha256` (flat), `text:sha256`.
pub fn cam_str(method: ContentAddressMethod, algo: HashAlgorithm) -> String {
    let prefix = match method {
        ContentAddressMethod::Text => "text:".to_string(),
        _ => format!("fixed:{}", method.file_ingestion_method().prefix()),
    };
    format!("{prefix}{}", algo.name())
}

/// Convenience: NAR/recursive sha256 add of a directory tree.
pub fn nar_sha256_method() -> (ContentAddressMethod, HashAlgorithm) {
    (
        ContentAddressMethod::NixArchive,
        HashAlgorithm::Sha256,
    )
}

/// Serialize a `vector<DerivedPath>` (worker-protocol `>= 1.30` form): count
/// then each as a length-prefixed string.
pub fn write_derived_paths(
    sink: &mut impl Write,
    store_dir: &StoreDir,
    paths: &[DerivedPath],
) -> io::Result<()> {
    wire::write_u64(sink, paths.len() as u64)?;
    for p in paths {
        wire::write_bytes(sink, p.to_string_legacy(store_dir).as_bytes())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FramedSink (port of serialise.hh FramedSink)
// ---------------------------------------------------------------------------

/// A write adapter that frames data as `[u64 len][len bytes]*` terminated by a
/// zero-length frame, matching C++ `FramedSink`/`FramedSource`.
///
/// Buffers up to `CHUNK` bytes before emitting a frame (mirroring the C++
/// `BufferedSink` behaviour).
pub struct FramedSink<'a> {
    inner: &'a UnixStream,
    buf: Vec<u8>,
}

const FRAME_CHUNK: usize = 64 * 1024;

impl<'a> FramedSink<'a> {
    /// Create a framed sink writing to `inner`.
    pub fn new(inner: &'a UnixStream) -> Self {
        FramedSink { inner, buf: Vec::with_capacity(FRAME_CHUNK) }
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let mut w = self.inner;
            wire::write_u64(&mut w, self.buf.len() as u64)?;
            w.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Flush any pending frame and write the zero-length terminator.
    pub fn finish(mut self) -> io::Result<()> {
        self.flush_frame()?;
        let mut w = self.inner;
        wire::write_u64(&mut w, 0)?;
        w.flush()?;
        Ok(())
    }
}

impl Write for FramedSink<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= FRAME_CHUNK {
            self.flush_frame()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_frame()?;
        (self.inner).flush()
    }
}
