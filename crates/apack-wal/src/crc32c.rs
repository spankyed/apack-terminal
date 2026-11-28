//! Pure-Rust Castagnoli CRC32C (`0x82F63B78` reflected polynomial).
//!
//! Zero dependencies and zero `unsafe`, verified against the RFC 3720 / iSCSI standard test vector
//! (`crc32c(b"123456789") == 0xE306_9283`).

const CASTAGNOLI_POLY: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ CASTAGNOLI_POLY;
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

const TABLE: [u32; 256] = build_table();

/// Computes the Castagnoli CRC32C checksum over `bytes`.
pub fn crc32c(bytes: &[u8]) -> u32 {
    crc32c_append(0, bytes)
}

/// Extends an existing CRC32C checksum with `bytes`.
pub fn crc32c_append(initial: u32, bytes: &[u8]) -> u32 {
    let mut crc = !initial;
    for &byte in bytes {
        let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    !crc
}
