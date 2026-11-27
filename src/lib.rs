pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod graph;
pub(crate) mod http;
pub mod img_preview;
pub mod mcp;
pub mod models;
pub mod parser;
pub mod resolver;
pub mod serve;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod ui;
pub mod updater;

/// Encodes bytes as lowercase hexadecimal.
pub fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Generates an RFC 4122 version 4 UUID string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`).
pub fn new_uuid_v4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3],
        b[4], b[5],
        b[6], b[7],
        b[8], b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod util_tests {
    use super::*;

    #[test]
    fn test_hex_encode() {
        assert_eq!(hex_encode([]), "");
        assert_eq!(hex_encode([0x00, 0x0a, 0x7f, 0xff]), "000a7fff");
    }

    #[test]
    fn test_new_uuid_v4_format_and_uniqueness() {
        let a = new_uuid_v4();
        let b = new_uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let bytes = a.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert_eq!(bytes[14], b'4');
        assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b'));
        assert!(a
            .bytes()
            .enumerate()
            .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) || matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    }
}
