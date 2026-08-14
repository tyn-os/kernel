//! Pure cpio "newc" parsing — no kernel deps, host-unit-testable (tests/unit/).
//!
//! This is remote-ish input: the embedded archive is trusted, but the parser is
//! the shape of code that must never panic or read out of bounds on a malformed
//! or truncated archive. Every field access is bounds-checked and every size
//! computation is overflow-checked; any malformation returns `None`, never a
//! panic or OOB read. Verus-tractable (clear invariant: output offsets are always
//! within `data`). See directions/PHASE2_LAYER1_UNIT.md.

/// Parse a fixed-width hex-ASCII cpio header field.
pub fn parse_hex(bytes: &[u8]) -> u64 {
    let mut val = 0u64;
    for &b in bytes {
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u64,
            b'a'..=b'f' => (b - b'a' + 10) as u64,
            b'A'..=b'F' => (b - b'A' + 10) as u64,
            _ => 0,
        };
        val = (val << 4) | digit;
    }
    val
}

/// Look up `path` in a newc cpio `data`. Returns `(data_offset, data_len)` for a
/// match, else `None`. Returns `None` (never panics / reads OOB) on any malformed
/// or truncated archive: short header, bad magic, zero name size, or a name/data
/// region that runs past the buffer or overflows a `usize`.
pub fn lookup(data: &[u8], path: &[u8]) -> Option<(usize, usize)> {
    // Normalize a leading "/" or "./" off the requested path.
    let normalized: &[u8] = if let Some(r) = path.strip_prefix(b"/") {
        r
    } else if let Some(r) = path.strip_prefix(b"./") {
        r
    } else {
        path
    };

    let mut offset = 0usize;
    loop {
        // A newc header is 110 bytes; need all of it present.
        let hdr_end = offset.checked_add(110)?;
        if hdr_end > data.len() {
            return None;
        }
        // Magic. `data[offset..offset+6]` is in-bounds because hdr_end <= len.
        if &data[offset..offset + 6] != b"070701" {
            return None;
        }
        let filesize = parse_hex(&data[offset + 54..offset + 62]) as usize;
        let namesize = parse_hex(&data[offset + 94..offset + 102]) as usize;
        // Names include a trailing NUL, so a valid namesize is >= 1. Zero would
        // underflow `namesize - 1` below.
        if namesize == 0 {
            return None;
        }

        let name_start = offset + 110; // <= data.len()
        let name_end = name_start.checked_add(namesize)?.checked_sub(1)?; // exclude NUL
        let data_start = name_start.checked_add(namesize)?.checked_add(3)? & !3;
        let data_end = data_start.checked_add(filesize)?;
        if name_end > data.len() || data_end > data.len() {
            return None;
        }

        let entry_name = &data[name_start..name_end];
        if entry_name == b"TRAILER!!!" {
            return None;
        }
        if entry_name == normalized {
            return Some((data_start, filesize));
        }

        offset = data_end.checked_add(3)? & !3; // 4-byte align to next entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    // A newc header is "070701" + 13 fixed 8-char hex fields (104 bytes) = 110.
    // Field 6 (offset 54) is filesize; field 12 (offset 94) is namesize.
    fn hex8(v: u64) -> Vec<u8> {
        format!("{:08X}", v as u32).into_bytes()
    }

    // Build one newc entry: 110-byte header + NUL-terminated name + data, each of
    // (header+name) and data padded to a 4-byte boundary. The `*_field` overrides
    // let a test lie about namesize/filesize to exercise the malformed paths.
    fn entry(name: &[u8], data: &[u8], namesize_field: Option<u64>, filesize_field: Option<u64>) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"070701");
        let mut fields = [0u64; 13];
        fields[6] = filesize_field.unwrap_or(data.len() as u64); // c_filesize @ offset 54
        fields[11] = namesize_field.unwrap_or(name.len() as u64 + 1); // c_namesize @ offset 94
        for f in fields.iter() {
            v.extend_from_slice(&hex8(*f));
        }
        assert_eq!(v.len(), 110);
        v.extend_from_slice(name);
        v.push(0);
        while v.len() % 4 != 0 { v.push(0); }
        v.extend_from_slice(data);
        while v.len() % 4 != 0 { v.push(0); }
        v
    }
    fn trailer() -> Vec<u8> {
        entry(b"TRAILER!!!", b"", None, None)
    }

    #[test]
    fn finds_a_real_file() {
        let mut a = entry(b"foo/bar.txt", b"hello", None, None);
        a.extend_from_slice(&trailer());
        let (off, len) = lookup(&a, b"foo/bar.txt").unwrap();
        assert_eq!(len, 5);
        assert_eq!(&a[off..off + len], b"hello");
    }

    #[test]
    fn normalizes_leading_slash_and_dotslash() {
        let mut a = entry(b"app.js", b"x", None, None);
        a.extend_from_slice(&trailer());
        assert!(lookup(&a, b"/app.js").is_some());
        assert!(lookup(&a, b"./app.js").is_some());
    }

    #[test]
    fn absent_file_is_none() {
        let mut a = entry(b"a", b"1", None, None);
        a.extend_from_slice(&trailer());
        assert_eq!(lookup(&a, b"b"), None);
    }

    // --- the invariant boundaries: each of these panics/OOBs on an unchecked parser ---

    #[test]
    fn truncated_header_is_none_not_panic() {
        let a = entry(b"foo", b"data", None, None);
        for cut in 0..a.len().min(120) {
            // must never panic for any prefix length
            let _ = lookup(&a[..cut], b"foo");
        }
    }

    #[test]
    fn bad_magic_is_none() {
        let mut a = entry(b"foo", b"data", None, None);
        a[0] = b'X';
        assert_eq!(lookup(&a, b"foo"), None);
    }

    #[test]
    fn zero_namesize_is_none_not_underflow() {
        // namesize field = 0 would underflow `namesize - 1`.
        let a = entry(b"foo", b"data", Some(0), None);
        assert_eq!(lookup(&a, b"foo"), None);
    }

    #[test]
    fn oversized_name_runs_past_buffer_is_none() {
        let a = entry(b"foo", b"data", Some(0xFFFF), None);
        assert_eq!(lookup(&a, b"foo"), None);
    }

    #[test]
    fn huge_filesize_does_not_overflow() {
        let a = entry(b"foo", b"data", None, Some(u32::MAX as u64));
        // data_end overflow / past-buffer must yield None, not panic/wrap.
        assert_eq!(lookup(&a, b"foo"), None);
    }

    #[test]
    fn trailer_stops_the_scan() {
        let mut a = trailer();
        a.extend_from_slice(&entry(b"after", b"z", None, None)); // unreachable past trailer
        assert_eq!(lookup(&a, b"after"), None);
    }

    #[test]
    fn parse_hex_basics() {
        assert_eq!(parse_hex(b"00000000"), 0);
        assert_eq!(parse_hex(b"0000000A"), 10);
        assert_eq!(parse_hex(b"deadBEEF"), 0xDEADBEEF);
    }
}
