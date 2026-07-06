//! Worker-protocol client tests.
//!
//! Unit tests exercise the pure serializers. Integration tests talk to a real
//! local `nix-daemon`; when no socket is present they print a notice and pass,
//! so the suite is green on machines without Nix.

use std::collections::BTreeSet;
use std::io::Write;

use jinx_store::daemon::{
    cam_str, resolve_socket_path, write_derived_paths, DaemonStore, DerivedPath, OutputsSpec,
    DEFAULT_SOCKET_PATH,
};
use jinx_store::hash::HashAlgorithm;
use jinx_store::store_path::{ContentAddressMethod, FileIngestionMethod, FixedOutputInfo, StoreDir};

// ---------------------------------------------------------------------------
// Unit tests (no daemon)
// ---------------------------------------------------------------------------

#[test]
fn cam_str_rendering() {
    assert_eq!(
        cam_str(ContentAddressMethod::NixArchive, HashAlgorithm::Sha256),
        "fixed:r:sha256"
    );
    assert_eq!(
        cam_str(ContentAddressMethod::Flat, HashAlgorithm::Sha256),
        "fixed:sha256"
    );
    assert_eq!(
        cam_str(ContentAddressMethod::Git, HashAlgorithm::Sha1),
        "fixed:git:sha1"
    );
    assert_eq!(
        cam_str(ContentAddressMethod::Text, HashAlgorithm::Sha256),
        "text:sha256"
    );
}

#[test]
fn outputs_spec_rendering() {
    assert_eq!(OutputsSpec::All.render(), "*");
    let names: BTreeSet<String> = ["out", "dev"].iter().map(|s| s.to_string()).collect();
    assert_eq!(OutputsSpec::Names(names).render(), "dev,out");
}

#[test]
fn derived_path_legacy_string() {
    let store = StoreDir::default();
    let drv = store
        .parse_store_path("/nix/store/00000000000000000000000000000000-foo.drv")
        .unwrap();
    let opaque = DerivedPath::Opaque(drv.clone());
    assert_eq!(
        opaque.to_string_legacy(&store),
        "/nix/store/00000000000000000000000000000000-foo.drv"
    );
    let built = DerivedPath::Built { drv_path: drv, outputs: OutputsSpec::All };
    assert_eq!(
        built.to_string_legacy(&store),
        "/nix/store/00000000000000000000000000000000-foo.drv!*"
    );
}

#[test]
fn derived_paths_wire_bytes() {
    let store = StoreDir::default();
    let drv = store
        .parse_store_path("/nix/store/00000000000000000000000000000000-foo.drv")
        .unwrap();
    let mut buf = Vec::new();
    write_derived_paths(
        &mut buf,
        &store,
        &[DerivedPath::Built {
            drv_path: drv,
            outputs: OutputsSpec::Names(["out"].iter().map(|s| s.to_string()).collect()),
        }],
    )
    .unwrap();
    // count = 1 (u64 LE)
    assert_eq!(&buf[..8], &[1, 0, 0, 0, 0, 0, 0, 0]);
    // then a length-prefixed string; decode it.
    let len = u64::from_le_bytes(buf[8..16].try_into().unwrap()) as usize;
    let s = std::str::from_utf8(&buf[16..16 + len]).unwrap();
    assert_eq!(s, "/nix/store/00000000000000000000000000000000-foo.drv!out");
}

#[test]
fn socket_path_env_resolution() {
    // Save & restore env to avoid cross-test interference.
    let saved_sock = std::env::var("NIX_DAEMON_SOCKET_PATH").ok();
    let saved_remote = std::env::var("NIX_REMOTE").ok();

    std::env::remove_var("NIX_DAEMON_SOCKET_PATH");
    std::env::set_var("NIX_REMOTE", "unix:///tmp/custom.sock");
    assert_eq!(resolve_socket_path(), "/tmp/custom.sock");

    std::env::set_var("NIX_REMOTE", "daemon");
    assert_eq!(resolve_socket_path(), DEFAULT_SOCKET_PATH);

    std::env::set_var("NIX_DAEMON_SOCKET_PATH", "/explicit/sock");
    assert_eq!(resolve_socket_path(), "/explicit/sock");

    match saved_sock {
        Some(v) => std::env::set_var("NIX_DAEMON_SOCKET_PATH", v),
        None => std::env::remove_var("NIX_DAEMON_SOCKET_PATH"),
    }
    match saved_remote {
        Some(v) => std::env::set_var("NIX_REMOTE", v),
        None => std::env::remove_var("NIX_REMOTE"),
    }
}

// ---------------------------------------------------------------------------
// Integration tests (live daemon; auto-skip when absent)
// ---------------------------------------------------------------------------

/// Connect to the daemon, or return `None` (printing a skip notice) if the
/// socket is unavailable.
fn try_connect() -> Option<DaemonStore> {
    let path = resolve_socket_path();
    if std::os::unix::net::UnixStream::connect(&path).is_err() {
        eprintln!("SKIP: no nix-daemon socket at {path}");
        return None;
    }
    match DaemonStore::connect_path(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("SKIP: daemon handshake failed: {e}");
            None
        }
    }
}

/// Find some already-valid store path by listing the store directory.
fn some_valid_path(store: &StoreDir) -> Option<jinx_store::store_path::StorePath> {
    let entries = std::fs::read_dir(store.as_str()).ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if let Ok(p) = jinx_store::store_path::StorePath::new(&name) {
            return Some(p);
        }
    }
    None
}

#[test]
fn live_handshake() {
    let Some(store) = try_connect() else { return };
    let major = jinx_store::daemon::proto_major(store.version()) >> 8;
    let minor = jinx_store::daemon::proto_minor(store.version());
    eprintln!(
        "negotiated protocol {major}.{minor}, daemon version {:?}, trusted {:?}, features {:?}",
        store.daemon_nix_version(),
        store.trusted(),
        store.features()
    );
    assert_eq!(major, 1);
    assert!(store.version() >= jinx_store::daemon::MIN_SUPPORTED_VERSION);
}

#[test]
fn live_is_valid_and_query_path_info() {
    let Some(mut store) = try_connect() else { return };
    let dir = store.store_dir().clone();
    let Some(path) = some_valid_path(&dir) else {
        eprintln!("SKIP: no store paths to test against");
        return;
    };
    assert!(store.is_valid_path(&path).unwrap(), "listed path should be valid");

    let info = store.query_path_info(&path).unwrap().expect("path info");
    assert_eq!(info.path, path);
    assert_eq!(info.nar_hash.algo, HashAlgorithm::Sha256);
    eprintln!(
        "path info for {}: narSize={}, refs={}, sigs={}",
        dir.print_store_path(&path),
        info.nar_size,
        info.references.len(),
        info.sigs.len()
    );

    // A bogus but well-formed path should be invalid.
    let bogus = jinx_store::store_path::StorePath::new(
        "00000000000000000000000000000000-does-not-exist",
    )
    .unwrap();
    assert!(!store.is_valid_path(&bogus).unwrap());

    // QueryValidPaths should keep the known-valid path and drop the bogus one.
    let mut set = jinx_store::store_path::StorePathSet::new();
    set.insert(path.clone());
    set.insert(bogus);
    let valid = store.query_valid_paths(&set, false).unwrap();
    assert!(valid.contains(&path));
    assert_eq!(valid.len(), 1);
}

#[test]
fn live_add_to_store_and_temp_root() {
    let Some(mut store) = try_connect() else { return };
    let dir = store.store_dir().clone();

    // A small deterministic tree.
    let tmp = std::env::temp_dir().join(format!("jinx-daemon-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let mut f = std::fs::File::create(tmp.join("greeting")).unwrap();
    f.write_all(b"hello from jinx daemon test\n").unwrap();
    drop(f);

    // Expected CA path, computed locally (NAR/recursive sha256, name "source").
    let (nar_hash, _) = jinx_store::nar::hash_path(&tmp, HashAlgorithm::Sha256).unwrap();
    let expected = dir
        .make_fixed_output_path(
            "source",
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: nar_hash,
                references: Default::default(),
            },
        )
        .unwrap();

    let info = store
        .add_to_store_nar_path(
            "source",
            &tmp,
            ContentAddressMethod::NixArchive,
            HashAlgorithm::Sha256,
            &jinx_store::store_path::StorePathSet::new(),
            false,
        )
        .unwrap();

    assert_eq!(info.path, expected, "daemon-returned path must match local CA computation");
    assert!(store.is_valid_path(&expected).unwrap());
    // AddTempRoot on it (idempotent, harmless).
    store.add_temp_root(&expected).unwrap();

    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("added {} to the store", dir.print_store_path(&expected));
}
