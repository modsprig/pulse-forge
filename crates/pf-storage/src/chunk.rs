//! Chunk binary layout helpers: encode/decode of one snapshot record and
//! the chunk header, with compile-time size checks.

use pf_types::CounterSnapshot;

pub const CHUNK_MAGIC: u32 = 0x5046_4252; // "PFRB"
pub const CHUNK_VERSION: u16 = 1;
/// Fixed-layout record: 14 u64 slots (see `encode_record`).
pub const RECORD_U64S: usize = 14;
/// Rotation threshold: records per chunk before sealing.
pub const MAX_RECORDS_PER_CHUNK: usize = 8192;
/// Header: magic + version + flags + count + min + max (bytes).
pub const HEADER_BYTES: usize = 4 + 2 + 2 + 4 + 8 + 8;
/// Trailer: CRC-32.
pub const TRAILER_BYTES: usize = 4;

/// Sentinel for absent counters (u64::MAX cannot collide with real values
/// because real counters are far smaller; decode maps it back to `None`).
const ABSENT: u64 = u64::MAX;

// Bounded conversions and index arithmetic on fixed-size buffers.

pub fn encode_record(s: &CounterSnapshot, out: &mut Vec<u8>) {
    let fields: [u64; RECORD_U64S] = [
        s.mono_ns,
        s.cpu_user_ns.unwrap_or(ABSENT),
        s.cpu_kernel_ns.unwrap_or(ABSENT),
        s.resident_bytes.unwrap_or(ABSENT),
        s.resident_peak_bytes.unwrap_or(ABSENT),
        s.private_commit_bytes.unwrap_or(ABSENT),
        s.private_commit_peak_bytes.unwrap_or(ABSENT),
        s.io_read_ops.unwrap_or(ABSENT),
        s.io_write_ops.unwrap_or(ABSENT),
        s.io_read_bytes.unwrap_or(ABSENT),
        s.io_write_bytes.unwrap_or(ABSENT),
        u64::from(s.thread_count.unwrap_or(u32::MAX)),
        u64::from(s.handle_count.unwrap_or(u32::MAX)),
        s.page_faults.unwrap_or(ABSENT),
    ];
    for f in fields {
        out.extend_from_slice(&f.to_le_bytes());
    }
}

/// Decode one record starting at `*pos`; advances `*pos`.
// Index arithmetic on fixed-layout buffers bounded by the length check.
#[allow(clippy::arithmetic_side_effects)]
pub fn decode_record(buf: &[u8], pos: &mut usize) -> Result<CounterSnapshot, String> {
    let need = RECORD_U64S * 8;
    if buf.len() < *pos + need {
        return Err("truncated record".into());
    }
    let mut fields = [0u64; RECORD_U64S];
    for (i, f) in fields.iter_mut().enumerate() {
        let at = *pos + i * 8;
        *f = u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
    }
    *pos += need;
    let opt = |v: u64| (v != ABSENT).then_some(v);
    let opt32 = |v: u64| -> Option<u32> {
        if v == u64::from(u32::MAX) {
            None
        } else {
            u32::try_from(v).ok()
        }
    };
    Ok(CounterSnapshot {
        mono_ns: fields[0],
        cpu_user_ns: opt(fields[1]),
        cpu_kernel_ns: opt(fields[2]),
        resident_bytes: opt(fields[3]),
        resident_peak_bytes: opt(fields[4]),
        private_commit_bytes: opt(fields[5]),
        private_commit_peak_bytes: opt(fields[6]),
        io_read_ops: opt(fields[7]),
        io_write_ops: opt(fields[8]),
        io_read_bytes: opt(fields[9]),
        io_write_bytes: opt(fields[10]),
        thread_count: opt32(fields[11]),
        handle_count: opt32(fields[12]),
        page_faults: opt(fields[13]),
    })
}

pub fn encode_header(count: u32, min_ns: u64, max_ns: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&CHUNK_MAGIC.to_le_bytes());
    out.extend_from_slice(&CHUNK_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&min_ns.to_le_bytes());
    out.extend_from_slice(&max_ns.to_le_bytes());
}

/// Decode and validate a chunk header at `pos`.
pub fn decode_header(buf: &[u8]) -> Result<(u32, u64, u64), String> {
    if buf.len() < HEADER_BYTES {
        return Err("chunk shorter than header".into());
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().expect("4"));
    if magic != CHUNK_MAGIC {
        return Err(format!("bad magic 0x{magic:08X}"));
    }
    let version = u16::from_le_bytes(buf[4..6].try_into().expect("2"));
    if version != CHUNK_VERSION {
        return Err(format!("unsupported chunk version {version}"));
    }
    let count = u32::from_le_bytes(buf[8..12].try_into().expect("4"));
    let min_ns = u64::from_le_bytes(buf[12..20].try_into().expect("8"));
    let max_ns = u64::from_le_bytes(buf[20..28].try_into().expect("8"));
    Ok((count, min_ns, max_ns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip_preserves_absence() {
        let s = CounterSnapshot {
            mono_ns: 42,
            cpu_user_ns: Some(7),
            resident_bytes: None,
            thread_count: Some(3),
            handle_count: None,
            ..CounterSnapshot::default()
        };
        let mut buf = Vec::new();
        encode_record(&s, &mut buf);
        assert_eq!(buf.len(), RECORD_U64S * 8);
        let mut pos = 0;
        let back = decode_record(&buf, &mut pos).unwrap();
        assert_eq!(back, s);
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn header_roundtrip_and_rejects_garbage() {
        let mut buf = Vec::new();
        encode_header(3, 10, 30, &mut buf);
        assert_eq!(buf.len(), HEADER_BYTES);
        assert_eq!(decode_header(&buf).unwrap(), (3, 10, 30));
        let mut bad = buf.clone();
        bad[0] ^= 0xFF;
        assert!(decode_header(&bad).is_err());
        assert!(decode_header(&bad[..10]).is_err());
    }
}
