//! Device identity, setup PIN derivation, and dynamic HTTP Basic Auth verification.
//!
//! Replaces legacy hardcoded credentials with a hardware-derived PIN based on the
//! station MAC address / eFuse, ensuring every board has a unique default setup credential.

pub fn portal_password_from_mac(mac: [u8; 6]) -> String {
    format!("Aether#{:02X}{:02X}{:02X}!", mac[3], mac[4], mac[5])
}

pub fn simple_base64_encode(input: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };

        out.push(CHARSET[(b0 >> 2) as usize] as char);
        out.push(CHARSET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARSET[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARSET[(b2 & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn verify_portal_basic_auth(auth_header: &str, expected_password: &str) -> bool {
    let Some(encoded) = auth_header.strip_prefix("Basic ") else {
        return false;
    };
    let expected_payload = format!("admin:{}", expected_password);
    let expected_b64 = simple_base64_encode(expected_payload.as_bytes());
    encoded.trim() == expected_b64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_portal_password_from_mac() {
        let mac = [0xAA, 0xBB, 0xCC, 0x12, 0x34, 0x56];
        assert_eq!(portal_password_from_mac(mac), "Aether#123456!");
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(simple_base64_encode(b"admin:secret"), "YWRtaW46c2VjcmV0");
    }

    #[test]
    fn test_verify_portal_basic_auth() {
        let pass = "secret123";
        let valid_header = format!("Basic {}", simple_base64_encode(b"admin:secret123"));
        assert!(verify_portal_basic_auth(&valid_header, pass));
        assert!(!verify_portal_basic_auth("Basic wrong", pass));
        assert!(!verify_portal_basic_auth("Bearer token", pass));
    }
}
