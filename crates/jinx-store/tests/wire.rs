//! Unit tests for the daemon protocol framing primitives.

use std::io::Cursor;

use jinx_store::wire;

#[test]
fn u64_roundtrip() {
    let mut buf = Vec::new();
    wire::write_u64(&mut buf, 0x0102030405060708).unwrap();
    assert_eq!(buf, [8, 7, 6, 5, 4, 3, 2, 1]); // little endian
    assert_eq!(wire::read_u64(&mut Cursor::new(&buf)).unwrap(), 0x0102030405060708);
}

#[test]
fn bytes_padding() {
    // empty string: just the length word
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"").unwrap();
    assert_eq!(buf, [0u8; 8]);

    // "x": length 1, payload, 7 zero bytes of padding
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"x").unwrap();
    assert_eq!(buf.len(), 16);
    assert_eq!(&buf[..8], &[1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(buf[8], b'x');
    assert_eq!(&buf[9..], &[0u8; 7]);

    // exactly 8 bytes: no padding
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"12345678").unwrap();
    assert_eq!(buf.len(), 16);

    // 9 bytes: 7 bytes padding
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"123456789").unwrap();
    assert_eq!(buf.len(), 24);

    for s in [&b""[..], b"x", b"1234567", b"12345678", b"123456789"] {
        let mut buf = Vec::new();
        wire::write_bytes(&mut buf, s).unwrap();
        assert_eq!(buf.len() % 8, 0);
        let mut cur = Cursor::new(&buf);
        assert_eq!(wire::read_bytes(&mut cur, 1 << 20).unwrap(), s);
        assert_eq!(cur.position() as usize, buf.len());
    }
}

#[test]
fn nonzero_padding_rejected() {
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"x").unwrap();
    let last = buf.len() - 1;
    buf[last] = 1; // corrupt the padding
    assert!(wire::read_bytes(&mut Cursor::new(&buf), 1 << 20).is_err());
}

#[test]
fn too_long_rejected() {
    let mut buf = Vec::new();
    wire::write_bytes(&mut buf, b"hello world").unwrap();
    assert!(wire::read_bytes(&mut Cursor::new(&buf), 4).is_err());
}

#[test]
fn string_list_roundtrip() {
    let items: Vec<&[u8]> = vec![b"out", b"dev", b"", b"with space"];
    let mut buf = Vec::new();
    wire::write_bytes_list(&mut buf, &items).unwrap();
    assert_eq!(buf.len() % 8, 0);
    let mut cur = Cursor::new(&buf);
    let back = wire::read_bytes_list(&mut cur, 1 << 20).unwrap();
    assert_eq!(back, items);
    assert_eq!(cur.position() as usize, buf.len());

    // empty list
    let mut buf = Vec::new();
    wire::write_bytes_list::<&[u8]>(&mut buf, &[]).unwrap();
    assert_eq!(buf, [0u8; 8]);
}
