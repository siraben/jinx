//! Daemon protocol framing primitives.
//!
//! Port of the low-level serializers in `src/libutil/serialise.cc`:
//! unsigned 64-bit little-endian integers, length-prefixed byte strings
//! padded with zeros to a multiple of 8 bytes, and string lists.
//! (The full daemon client comes in a later milestone.)

use std::io::{self, Read, Write};

/// Error message analogue of C++ `SerialisationError`.
fn ser_err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("serialisation error: {msg}"))
}

/// Write a u64 as 8 little-endian bytes.
pub fn write_u64(sink: &mut impl Write, n: u64) -> io::Result<()> {
    sink.write_all(&n.to_le_bytes())
}

/// Read a little-endian u64.
pub fn read_u64(source: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    source.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

/// Port of `writePadding`: zero-pad `len` bytes of payload to a multiple
/// of 8.
pub fn write_padding(sink: &mut impl Write, len: u64) -> io::Result<()> {
    if len % 8 != 0 {
        let zero = [0u8; 8];
        sink.write_all(&zero[..(8 - (len % 8) as usize)])?;
    }
    Ok(())
}

/// Port of `readPadding`: consume and verify the zero padding for `len`
/// payload bytes.
pub fn read_padding(source: &mut impl Read, len: u64) -> io::Result<()> {
    if len % 8 != 0 {
        let n = 8 - (len % 8) as usize;
        let mut buf = [0u8; 8];
        source.read_exact(&mut buf[..n])?;
        if buf.iter().any(|&b| b != 0) {
            return Err(ser_err("non-zero padding"));
        }
    }
    Ok(())
}

/// Port of `writeString` / `operator<<(Sink, string_view)`: u64 length,
/// payload, zero padding.
pub fn write_bytes(sink: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u64(sink, data.len() as u64)?;
    sink.write_all(data)?;
    write_padding(sink, data.len() as u64)
}

/// Port of `readString` (unbounded variant used with an explicit `max`).
pub fn read_bytes(source: &mut impl Read, max: u64) -> io::Result<Vec<u8>> {
    let len = read_u64(source)?;
    if len > max {
        return Err(ser_err("string is too long"));
    }
    let mut buf = vec![0u8; len as usize];
    source.read_exact(&mut buf)?;
    read_padding(source, len)?;
    Ok(buf)
}

/// Port of `writeStrings`: u64 count followed by each string.
pub fn write_bytes_list<S: AsRef<[u8]>>(sink: &mut impl Write, items: &[S]) -> io::Result<()> {
    write_u64(sink, items.len() as u64)?;
    for item in items {
        write_bytes(sink, item.as_ref())?;
    }
    Ok(())
}

/// Port of `readStrings`.
pub fn read_bytes_list(source: &mut impl Read, max_each: u64) -> io::Result<Vec<Vec<u8>>> {
    let count = read_u64(source)?;
    let mut res = Vec::with_capacity(count.min(4096) as usize);
    for _ in 0..count {
        res.push(read_bytes(source, max_each)?);
    }
    Ok(res)
}
