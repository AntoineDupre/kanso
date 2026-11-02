use std::fmt::Write as _;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use crate::error::{Error, Result};

/// Fill `buffer` from the operating system's cryptographically secure source.
pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::msg(format!("random source failed: {error}")))?;
    Ok(bytes)
}

/// URL-safe base64 without padding, as used by tokens and session ids.
pub fn base64_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode_base64_url(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok()
}

/// A random RFC 4122 version 4 UUID.
pub fn uuid_v4() -> Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

/// A new API token: `kanso_` followed by 32 random bytes in base64url (43 chars).
pub fn api_token() -> Result<String> {
    Ok(format!("kanso_{}", base64_url(&random_bytes::<32>()?)))
}

/// SHA-256 of a value, hex encoded (lowercase) for storage.
pub fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Is this a canonical hyphenated UUID?
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_have_the_expected_shape() {
        let id = uuid_v4().expect("uuid");
        assert_eq!(id.len(), 36);
        assert!(is_uuid(&id));
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));

        assert!(is_uuid("00000000-0000-4000-8000-000000000000"));
        assert!(!is_uuid("00000000-0000-4000-8000-00000000000"));
        assert!(!is_uuid("00000000_0000_4000_8000_000000000000"));
        assert!(!is_uuid("not-a-uuid"));
    }

    #[test]
    fn tokens_have_the_documented_format() {
        let token = api_token().expect("token");
        assert!(token.starts_with("kanso_"));
        assert_eq!(token.len(), "kanso_".len() + 43);
        assert!(token["kanso_".len()..]
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'));
        assert_ne!(token, api_token().expect("second token"));
    }

    #[test]
    fn hashes_are_lowercase_hex_sha256() {
        let hash = sha256_hex("abc");
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex("abc"), hash);
        assert_ne!(sha256_hex("abd"), hash);
    }

    #[test]
    fn base64url_round_trips_without_padding() {
        let encoded = base64_url(b"hello world");
        assert_eq!(encoded, "aGVsbG8gd29ybGQ");
        assert_eq!(
            decode_base64_url(&encoded).expect("decode"),
            b"hello world".to_vec()
        );
        assert!(decode_base64_url("!!!!").is_none());
    }
}
