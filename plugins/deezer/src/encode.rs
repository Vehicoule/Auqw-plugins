//! Percent-encoding of UTF-8 query bytes for the `q` parameter.

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode every byte outside the RFC 3986 unreserved set with
/// uppercase hex. Non-ASCII input encodes as its UTF-8 bytes.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreserved_passes_through() {
        assert_eq!(percent_encode("abcXYZ-._~019"), "abcXYZ-._~019");
    }

    #[test]
    fn space_and_reserved_encode() {
        assert_eq!(percent_encode("a b&c+d=e/f?"), "a%20b%26c%2Bd%3De%2Ff%3F");
    }

    #[test]
    fn non_latin_encodes_utf8_bytes() {
        assert_eq!(percent_encode("夜の歌"), "%E5%A4%9C%E3%81%AE%E6%AD%8C");
    }
}
