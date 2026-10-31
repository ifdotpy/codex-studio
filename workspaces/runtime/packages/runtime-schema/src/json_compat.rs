//! Canonical legacy JSON spellings used by durable receipt identities.
use serde_json::Value;

mod serializer;

/// Python-compatible sorted-key JSON used for persisted signatures.
pub fn python_json(value: &Value) -> String {
    python_json_serialized(value).expect("JSON values are serializable")
}

/// Serialize typed data with Python-compatible sorted keys and ASCII escaping.
pub fn python_json_serialized<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<String, serde_json::Error> {
    serializer::serialize(value, ", ", ": ", true)
}

/// Sorted UTF-8 JSON with Python's human-readable separators.
pub fn python_json_utf8(value: &Value) -> String {
    serializer::serialize(value, ", ", ": ", false).expect("JSON values are serializable")
}

/// Compact Python-compatible JSON for content-addressed signatures.
pub fn python_json_compact(value: &Value) -> String {
    serializer::serialize(value, ",", ":", false).expect("JSON values are serializable")
}

/// Sorted ASCII JSON with explicitly selected separators for provider catalog digests.
pub fn python_json_with_separators(value: &Value, item: &str, key: &str) -> String {
    serializer::serialize(value, item, key, true).expect("JSON values are serializable")
}

/// Quote a string using Python JSON escaping.
pub fn python_quote(value: &str, ensure_ascii: bool) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            c if c < ' ' || (ensure_ascii && c as u32 == 0x7f) => {
                output.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if ensure_ascii && c as u32 > 0x7f => {
                let n = c as u32;
                if n <= 0xffff {
                    output.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 0x10000;
                    output.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 0x3ff)
                    ));
                }
            }
            c => output.push(c),
        }
    }
    output.push('"');
    output
}

/// Serialize a finite float using the spelling emitted by Python's `json.dumps`.
pub fn python_float_json(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".to_owned()
        } else {
            "0.0".to_owned()
        };
    }

    let raw = format!("{value:?}");
    let (mantissa, exponent) = match raw.split_once('e') {
        Some((mantissa, exponent)) => match exponent.parse::<i32>() {
            Ok(exponent) => (mantissa, exponent),
            Err(_) => return raw,
        },
        None => (raw.as_str(), 0),
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (integer, fractional) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let mut digits = format!("{integer}{fractional}");
    let mut decimal_position = integer.len() as i32 + exponent;
    let leading_zeroes = digits.bytes().take_while(|digit| *digit == b'0').count();
    digits.drain(..leading_zeroes);
    decimal_position -= leading_zeroes as i32;
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let scientific_exponent = decimal_position - 1;

    let mut result = String::new();
    if negative {
        result.push('-');
    }
    if !(-4..16).contains(&scientific_exponent) {
        result.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            result.push('.');
            result.push_str(&digits[1..]);
        }
        result.push('e');
        result.push(if scientific_exponent < 0 { '-' } else { '+' });
        result.push_str(&format!("{:02}", scientific_exponent.unsigned_abs()));
    } else if decimal_position <= 0 {
        result.push_str("0.");
        result.extend(std::iter::repeat_n(
            '0',
            decimal_position.unsigned_abs() as usize,
        ));
        result.push_str(&digits);
    } else if decimal_position as usize >= digits.len() {
        result.push_str(&digits);
        result.extend(std::iter::repeat_n(
            '0',
            decimal_position as usize - digits.len(),
        ));
        result.push_str(".0");
    } else {
        let position = decimal_position as usize;
        result.push_str(&digits[..position]);
        result.push('.');
        result.push_str(&digits[position..]);
    }
    result
}

#[cfg(test)]
mod serialization_tests {
    use super::{
        python_float_json, python_json, python_json_compact, python_json_serialized,
        python_json_utf8, python_json_with_separators, python_quote,
    };
    use serde_json::Value;

    fn old_json(value: &Value, item: &str, key_separator: &str, ascii: bool) -> String {
        match value {
            Value::Null | Value::Bool(_) => value.to_string(),
            Value::Number(n) if n.is_f64() => n
                .as_f64()
                .map(python_float_json)
                .unwrap_or_else(|| n.to_string()),
            Value::Number(n) => n.to_string(),
            Value::String(s) => python_quote(s, ascii),
            Value::Array(items) => format!(
                "[{}]",
                items
                    .iter()
                    .map(|value| old_json(value, item, key_separator, ascii))
                    .collect::<Vec<_>>()
                    .join(item)
            ),
            Value::Object(items) => {
                let mut keys = items.keys().collect::<Vec<_>>();
                keys.sort();
                format!(
                    "{{{}}}",
                    keys.into_iter()
                        .map(|key| format!(
                            "{}{key_separator}{}",
                            python_quote(key, ascii),
                            old_json(&items[key], item, key_separator, ascii)
                        ))
                        .collect::<Vec<_>>()
                        .join(item)
                )
            }
        }
    }
    use serde_json::json;

    #[test]
    fn serialize_matches_existing_python_json_bytes() {
        for value in [
            json!({"z":"é😀\n", "a":[null, true, {}, []]}),
            json!({"large":1.0e20,"small":1.0e-7,"negative_zero":-0.0,"nested":{"z":2,"a":1}}),
            json!({"id":"c","author":"user","text":"  raw é  ","responses":[],"status":"open"}),
            json!({"id":"r","params":{"questions":[{"id":"1","question":"Why?"}]},"method":"agent/asyncQuestion"}),
        ] {
            assert_eq!(
                python_json_serialized(&value).unwrap(),
                old_json(&value, ", ", ": ", true)
            );
            assert_eq!(python_json(&value), old_json(&value, ", ", ": ", true));
            assert_eq!(
                python_json_utf8(&value),
                old_json(&value, ", ", ": ", false)
            );
            assert_eq!(
                python_json_compact(&value),
                old_json(&value, ",", ":", false)
            );
            assert_eq!(
                python_json_with_separators(&value, ";", "="),
                old_json(&value, ";", "=", true)
            );
        }
    }

    #[test]
    fn typed_record_matches_previous_value_serialization() {
        use serde::Serialize;
        #[derive(Serialize)]
        struct Record<'a> {
            z: &'a str,
            values: Vec<f64>,
            a: Option<&'a str>,
        }
        let record = Record {
            z: "é😀",
            values: vec![-0.0, 1e-7, 1e20],
            a: None,
        };
        let old = record.serialize(serde_json::value::Serializer).unwrap();
        assert_eq!(
            python_json_serialized(&record).unwrap(),
            old_json(&old, ", ", ": ", true)
        );
        let raw = serde_json::value::RawValue::from_string("{\"a\":1}".into()).unwrap();
        assert!(python_json_serialized(&raw).is_err());
    }
}
