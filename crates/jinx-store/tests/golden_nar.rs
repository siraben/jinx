//! Golden tests for the NAR serializer.
//!
//! `fixtures/nartree.nar` was produced by `nix nar pack` (Nix 2.33.3) over
//! the exact tree recreated below; the other goldens are sha256 hashes of
//! NARs produced by the same nix.

use std::fs;
use std::path::PathBuf;

use jinx_store::hash::{HashAlgorithm, HashFormat};
use jinx_store::nar;

/// Golden NAR bytes of the fixture tree.
const GOLDEN_TREE_NAR: &[u8] = include_bytes!("fixtures/nartree.nar");

/// Recreate the fixture tree:
///
/// ```text
/// nartree/
///   emptydir/
///   link -> ./regular.txt
///   nested/
///     deep.bin      ("deep")
///     empty         ("")
///   regular.txt     ("hello world\n")
///   script.sh       ("#!/bin/sh\necho hi\n", executable)
/// ```
fn make_tree(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jinx-store-nar-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join("nartree");
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::create_dir_all(root.join("emptydir")).unwrap();
    fs::write(root.join("regular.txt"), "hello world\n").unwrap();
    fs::write(root.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("./regular.txt", root.join("link")).unwrap();
    }
    fs::write(root.join("nested/empty"), "").unwrap();
    fs::write(root.join("nested/deep.bin"), "deep").unwrap();
    root
}

#[cfg(unix)]
#[test]
fn golden_tree_nar_bytes() {
    let root = make_tree("tree");
    let nar = nar::dump_path_to_vec(&root).unwrap();
    assert_eq!(nar.len(), 1464);
    assert_eq!(nar, GOLDEN_TREE_NAR);

    // streaming hash without materializing
    // (nix-hash --type sha256 nartree)
    let (h, size) = nar::hash_path(&root, HashAlgorithm::Sha256).unwrap();
    assert_eq!(size, 1464);
    assert_eq!(
        h.to_string(HashFormat::Base16, false),
        "7f1f067d1908976a7d001056e24169dc64326ff53e762af0bbc1e1a20f6ec56f"
    );

    fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn golden_single_file_and_symlink_nars() {
    let root = make_tree("single");

    // executable regular file (nix nar pack nartree/script.sh | sha256)
    let (h, _) = nar::hash_path(root.join("script.sh"), HashAlgorithm::Sha256).unwrap();
    assert_eq!(
        h.to_string(HashFormat::Base16, false),
        "5e0accf02cedede5e4119ffa15e79e79a5fb1fb9bc43c3d434f33227a14477a0"
    );

    // symlink (nix nar pack nartree/link | sha256), plus exact bytes
    let nar = nar::dump_path_to_vec(root.join("link")).unwrap();
    let (h, _) = nar::hash_path(root.join("link"), HashAlgorithm::Sha256).unwrap();
    assert_eq!(
        h.to_string(HashFormat::Base16, false),
        "ef901ab807dd7f4f352e412b3cd52f71c35aba904df24a057e0636a0e1c066df"
    );
    let mut expect = Vec::new();
    for tok in [
        &b"nix-archive-1"[..],
        b"(",
        b"type",
        b"symlink",
        b"target",
        b"./regular.txt",
        b")",
    ] {
        jinx_store::wire::write_bytes(&mut expect, tok).unwrap();
    }
    assert_eq!(nar, expect);

    fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[cfg(unix)]
#[test]
fn filtered_walk_supplies_entry_types() {
    use jinx_store::nar::NarFileType;
    use std::collections::BTreeMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let root = make_tree("filtered-types");
    let fifo = CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo` is a valid NUL-terminated path; the fixture path is fresh.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut seen = BTreeMap::new();
    let mut out = Vec::new();
    nar::dump_path_filtered(&root, &mut out, &mut |path, file_type| {
        seen.insert(
            path.file_name().unwrap().to_string_lossy().into_owned(),
            file_type,
        );
        // Unsupported file types are reported as "unknown" to the callback
        // and may be filtered out before the NAR walker rejects them.
        Ok(file_type != NarFileType::Unknown)
    })
    .unwrap();

    assert_eq!(out, GOLDEN_TREE_NAR);
    assert_eq!(seen["emptydir"], NarFileType::Directory);
    assert_eq!(seen["nested"], NarFileType::Directory);
    assert_eq!(seen["regular.txt"], NarFileType::Regular);
    assert_eq!(seen["script.sh"], NarFileType::Regular);
    assert_eq!(seen["link"], NarFileType::Symlink);
    assert_eq!(seen["pipe"], NarFileType::Unknown);

    fs::remove_dir_all(root.parent().unwrap()).unwrap();
}

#[test]
fn golden_dump_string() {
    // dump_string("x") must equal the NAR of a non-executable file
    // containing "x" (nix nar pack xfile | sha256).
    let mut nar = Vec::new();
    nar::dump_string(b"x", &mut nar).unwrap();
    let h = jinx_store::hash_string(HashAlgorithm::Sha256, &nar);
    assert_eq!(
        h.to_string(HashFormat::Base16, false),
        "2ca0b8ce996f865db37619bfe91023559305aad8158042fc6ddb0ef1d43c5b67"
    );
}

#[test]
fn golden_tree_a_nar_hash() {
    // tree/ { a = "hello\n" } — nix-hash --type sha256 tree
    let dir = std::env::temp_dir().join(format!("jinx-store-nar-test2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("tree")).unwrap();
    fs::write(dir.join("tree/a"), "hello\n").unwrap();
    let (h, _) = nar::hash_path(dir.join("tree"), HashAlgorithm::Sha256).unwrap();
    assert_eq!(
        h.to_string(HashFormat::Base16, false),
        "9589bea391d3a6d3ba88d186e8be3510c8461b28762fd82de4feecb491fc82a2"
    );
    fs::remove_dir_all(&dir).unwrap();
}
