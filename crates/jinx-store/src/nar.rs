//! NAR (Nix ARchive) serialization.
//!
//! Port of the dump side of `src/libutil/archive.cc`, byte-exact, with the
//! macOS "case hack" OFF (Nix's canonical NAR format).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::hash::{Hash, HashAlgorithm, HashSink};
use crate::wire;

/// `nix-archive-1`, the version magic at the start of every NAR.
pub const NAR_VERSION_MAGIC_1: &[u8] = b"nix-archive-1";

/// Maximum directory nesting depth (matches `narMaxDepth`).
const NAR_MAX_DEPTH: usize = 64;

fn other_err(msg: String) -> io::Error {
    io::Error::other(msg)
}

/// Serialize the file-system object at `path` as a NAR into `sink`.
///
/// Port of `SourceAccessor::dumpPath` (over the real file system, no path
/// filter, case hack off). Regular files, directories (entries sorted by
/// byte-wise name order) and symlinks are supported.
pub fn dump_path(path: impl AsRef<Path>, sink: &mut impl Write) -> io::Result<()> {
    wire::write_bytes(sink, NAR_VERSION_MAGIC_1)?;
    dump(path.as_ref(), sink, 0)
}

fn dump(path: &Path, sink: &mut impl Write, depth: usize) -> io::Result<()> {
    if depth >= NAR_MAX_DEPTH {
        return Err(other_err(format!(
            "path '{}' exceeds maximum NAR directory depth of {}",
            path.display(),
            NAR_MAX_DEPTH
        )));
    }

    let st = fs::symlink_metadata(path)?;
    let ft = st.file_type();

    wire::write_bytes(sink, b"(")?;

    if ft.is_file() {
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"regular")?;
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            st.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        if executable {
            wire::write_bytes(sink, b"executable")?;
            wire::write_bytes(sink, b"")?;
        }
        dump_contents(path, st.len(), sink)?;
    } else if ft.is_dir() {
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"directory")?;

        // Entries sorted by name, byte-wise (case hack OFF).
        let mut entries: BTreeMap<Vec<u8>, std::path::PathBuf> = BTreeMap::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            #[cfg(unix)]
            let name = {
                use std::os::unix::ffi::OsStrExt;
                entry.file_name().as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let name = entry.file_name().to_string_lossy().into_owned().into_bytes();
            entries.insert(name, entry.path());
        }

        for (name, entry_path) in &entries {
            wire::write_bytes(sink, b"entry")?;
            wire::write_bytes(sink, b"(")?;
            wire::write_bytes(sink, b"name")?;
            wire::write_bytes(sink, name)?;
            wire::write_bytes(sink, b"node")?;
            dump(entry_path, sink, depth + 1)?;
            wire::write_bytes(sink, b")")?;
        }
    } else if ft.is_symlink() {
        wire::write_bytes(sink, b"type")?;
        wire::write_bytes(sink, b"symlink")?;
        wire::write_bytes(sink, b"target")?;
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        let target_bytes = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
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

/// Port of `dumpContents`: `contents` tag, u64 size, raw bytes, zero
/// padding to 8 bytes.
fn dump_contents(path: &Path, size: u64, sink: &mut impl Write) -> io::Result<()> {
    wire::write_bytes(sink, b"contents")?;
    wire::write_u64(sink, size)?;
    let mut file = fs::File::open(path)?;
    let mut buf = [0u8; 65536];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        sink.write_all(&buf[..n])?;
    }
    if total != size {
        return Err(other_err(format!(
            "file '{}' changed size while dumping NAR",
            path.display()
        )));
    }
    wire::write_padding(sink, size)
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
    Ok(sink.finish())
}
