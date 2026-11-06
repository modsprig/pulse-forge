//! CRC-32 (IEEE 802.3) — dependency-free, table-free (bitwise), adequate
//! for chunk corruption detection at these sizes.

/// Compute CRC-32 (IEEE) over `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Reference vectors from the IEEE CRC-32 test suite.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn detects_single_bit_flips() {
        let data = b"pulseforge chunk payload 0123456789";
        let good = crc32(data);
        for i in 0..data.len() {
            let mut corrupted = data.to_vec();
            corrupted[i] ^= 0x01;
            assert_ne!(crc32(&corrupted), good, "bit flip at {i} undetected");
        }
    }
}
