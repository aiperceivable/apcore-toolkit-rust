// Body encoding helpers: `application/x-www-form-urlencoded` in both
// directions, plus the base64 needed for `client_secret_basic`.
//
// Written here rather than pulled in as dependencies: each is a dozen lines,
// and the crate's dependency policy is to evaluate necessity first. Both are
// exercised directly by the conformance corpus (cases 022 and 049).

/// Percent-encode one value for an `application/x-www-form-urlencoded` body.
///
/// Unreserved characters (RFC 3986 `A-Z a-z 0-9 - . _ ~`) pass through, a
/// space becomes `+`, and everything else is percent-encoded byte by byte.
///
/// Public because RFC 6749 section 2.3.1 requires the *same* encoding to be
/// applied to the client id and secret before they are base64'd into a Basic
/// header -- see [`basic_credentials`].
pub fn encode_form_component(value: &str) -> String {
    encode_component(value)
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decode one `application/x-www-form-urlencoded` component.
///
/// A malformed percent-escape is preserved verbatim rather than rejected: the
/// parser must fail soft on shape so a proprietary body still reaches the
/// operator as a diagnostic.
fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                // Slice the bytes, not the &str: the two characters after a
                // stray '%' may be part of a multi-byte sequence, and string
                // slicing on a non-boundary panics.
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encode ordered key/value pairs as an `application/x-www-form-urlencoded`
/// body. Insertion order is preserved so request bodies are reproducible
/// across SDKs.
pub fn form_encode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse an `application/x-www-form-urlencoded` body into ordered pairs.
///
/// Some authorization servers return form-encoded token responses unless the
/// request explicitly asks for JSON, so this is the mandated parse fallback,
/// not a convenience.
pub fn form_decode(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|segment| !segment.is_empty())
        .map(|segment| match segment.split_once('=') {
            Some((key, value)) => (decode_component(key), decode_component(value)),
            None => (decode_component(segment), String::new()),
        })
        .collect()
}

/// The credential portion of an RFC 6749 section 2.3.1 Basic header.
///
/// The client id and secret are **form-urlencoded first**, then joined with a
/// colon and base64'd. Skipping the encoding step works for the alphanumeric
/// values a conformance corpus can carry and fails for a real secret
/// containing `:`, `+`, a space, or a non-ASCII byte.
pub fn basic_credentials(client_id: &str, client_secret: &str) -> String {
    base64_encode(&format!(
        "{}:{}",
        encode_form_component(client_id),
        encode_form_component(client_secret)
    ))
}

/// Standard base64 with padding, for the `client_secret_basic` header.
pub fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode_matches_conformance_vector() {
        // Conformance case 049 pins this exact string.
        assert_eq!(base64_encode("cid:sec"), "Y2lkOnNlYw==");
    }

    #[test]
    fn test_basic_credentials_matches_conformance_vector() {
        assert_eq!(basic_credentials("cid", "sec"), "Y2lkOnNlYw==");
    }

    #[test]
    fn test_basic_credentials_form_encodes_before_base64() {
        // RFC 6749 section 2.3.1. A secret with a colon would otherwise make
        // the joined string ambiguous, and one with a space or a non-ASCII
        // byte would not survive the header at all.
        assert_eq!(
            basic_credentials("cid", "se:c"),
            base64_encode("cid:se%3Ac")
        );
        assert_eq!(basic_credentials("cid", "se c"), base64_encode("cid:se+c"));
        assert_eq!(
            basic_credentials("c+d", "s/c"),
            base64_encode("c%2Bd:s%2Fc")
        );
    }

    #[test]
    fn test_base64_encode_padding_lengths() {
        assert_eq!(base64_encode(""), "");
        assert_eq!(base64_encode("a"), "YQ==");
        assert_eq!(base64_encode("ab"), "YWI=");
        assert_eq!(base64_encode("abc"), "YWJj");
        assert_eq!(base64_encode("abcd"), "YWJjZA==");
    }

    #[test]
    fn test_form_encode_escapes_reserved_characters() {
        let pairs = vec![
            ("scope".to_string(), "openid api.read".to_string()),
            (
                "audience".to_string(),
                "https://api.example.com".to_string(),
            ),
        ];
        assert_eq!(
            form_encode(&pairs),
            "scope=openid+api.read&audience=https%3A%2F%2Fapi.example.com"
        );
    }

    #[test]
    fn test_form_decode_roundtrip() {
        let pairs = vec![
            ("a".to_string(), "one two".to_string()),
            ("b".to_string(), "x/y=z".to_string()),
        ];
        assert_eq!(form_decode(&form_encode(&pairs)), pairs);
    }

    #[test]
    fn test_form_decode_token_response() {
        // Conformance case 022.
        assert_eq!(
            form_decode("access_token=t&token_type=Bearer&expires_in=3600"),
            vec![
                ("access_token".to_string(), "t".to_string()),
                ("token_type".to_string(), "Bearer".to_string()),
                ("expires_in".to_string(), "3600".to_string()),
            ]
        );
    }

    #[test]
    fn test_form_decode_valueless_and_malformed_segments() {
        assert_eq!(
            form_decode("flag&bad=%ZZ"),
            vec![
                ("flag".to_string(), String::new()),
                ("bad".to_string(), "%ZZ".to_string()),
            ]
        );
    }
}
