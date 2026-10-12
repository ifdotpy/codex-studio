use serde_json::Value;

const MAX_NESTING: usize = 127;

/// Bound bytes and nesting before serde_json's recursive value parser runs.
pub(crate) fn parse_bounded(bytes: &[u8], byte_limit: usize) -> Result<Value, ()> {
    if bytes.len() > byte_limit || nesting_exceeds(bytes, MAX_NESTING) {
        return Err(());
    }
    serde_json::from_slice(bytes).map_err(|_| ())
}

fn nesting_exceeds(bytes: &[u8], limit: usize) -> bool {
    let mut depth = 0usize;
    let mut string = false;
    let mut escape = false;
    for byte in bytes {
        if string {
            if escape {
                escape = false;
            } else if *byte == b'\\' {
                escape = true;
            } else if *byte == b'"' {
                string = false;
            }
            continue;
        }
        match *byte {
            b'"' => string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::parse_bounded;

    #[test]
    fn nesting_and_size_are_bounded_before_json_parsing() {
        let deep = format!("{}0{}", "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(deep.len(), 20_001);
        assert!(parse_bounded(deep.as_bytes(), 256 * 1024).is_err());
        assert!(parse_bounded(br#"{"value":"[ {{{ \""}"#, 64).is_ok());
        assert!(parse_bounded(b"{}", 1).is_err());
        for depth in [127, 128, 129] {
            let nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(parse_bounded(nested.as_bytes(), 1024).is_ok(), depth == 127);
        }
    }

    #[test]
    fn serde_private_number_key_remains_an_object_in_raw_json() {
        let value = parse_bounded(br#"{"$serde_json::private::Number":"1"}"#, 1024)
            .expect("valid JSON object");
        assert_eq!(
            value,
            serde_json::json!({"$serde_json::private::Number":"1"})
        );
    }

    #[test]
    fn duplicate_object_keys_use_the_last_value() {
        let value = parse_bounded(br#"{"value":1,"value":2}"#, 1024)
            .expect("duplicate keys are valid JSON input");
        assert_eq!(value, serde_json::json!({"value": 2}));
    }

    #[test]
    fn integer_outside_f64_range_is_a_bounded_parse_error() {
        let body = format!("{{\"before\":{}}}", "9".repeat(401));
        assert!(parse_bounded(body.as_bytes(), 1024).is_err());
    }
}
