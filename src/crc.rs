//! IEEE CRC-32 (ISO-HDLC), the checksum used on page trailers.
//!
//! Polynomial `0xEDB88320`, init `0xFFFFFFFF`, xorout `0xFFFFFFFF`.
//! The table is built in a `const fn`, so there is no codegen crate and no
//! lazy init.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = 0xEDB8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const TABLE: [u32; 256] = make_table();

/// Checksum of `data`. Known answer: `crc32(b"123456789") == 0xCBF4_3926`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn iso_hdlc_known_answer() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn bit_flip_changes_the_sum() {
        assert_ne!(crc32(&[0x00]), crc32(&[0x01]));
    }
}
