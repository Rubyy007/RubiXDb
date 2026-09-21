//! Base64 key/value encoding helpers — `PHASE_API_ARCHITECTURE.md` §2:
//! keys and values are arbitrary `&[u8]` at the engine boundary, never
//! assumed UTF-8, so every wire representation goes through standard,
//! padded base64.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use crate::error::ApiError;

pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn decode(field_name: &str, b64: &str) -> Result<Vec<u8>, ApiError> {
    STANDARD
        .decode(b64)
        .map_err(|e| ApiError::Validation(format!("{field_name} is not valid base64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_bytes() {
        let bytes = vec![0u8, 255, 1, 2, 3, 254, 253];
        let encoded = encode(&bytes);
        let decoded = decode("key_b64", &encoded).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn rejects_invalid_base64() {
        let err = decode("key_b64", "not!!valid!!base64").unwrap_err();
        assert!(matches!(err, ApiError::Validation(_)));
    }
}
