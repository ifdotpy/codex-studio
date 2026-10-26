//! Canonical legacy JSON spellings used by durable receipt identities.
use serde_json::Value;

/// Python-compatible sorted-key JSON used for persisted signatures.
pub fn python_json(value: &Value) -> String {
    python_json_separated(value, ", ", ": ", true)
}

/// Sorted UTF-8 JSON with Python's human-readable separators.
pub fn python_json_utf8(value: &Value) -> String {
    python_json_separated(value, ", ", ": ", false)
}

/// Compact Python-compatible JSON for content-addressed signatures.
pub fn python_json_compact(value: &Value) -> String {
    python_json_separated(value, ",", ":", false)
}

/// Sorted ASCII JSON with explicitly selected separators for provider catalog digests.
pub fn python_json_with_separators(value: &Value, item: &str, key: &str) -> String {
    python_json_separated(value, item, key, true)
}

fn python_json_separated(
    value: &Value,
    item_separator: &str,
    key_separator: &str,
    ensure_ascii: bool,
) -> String {
    match value {
        Value::Null | Value::Bool(_) => value.to_string(),
        Value::Number(number) if number.is_f64() => number
            .as_f64()
            .map(python_float_json)
            .unwrap_or_else(|| number.to_string()),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_quote(text, ensure_ascii),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(|item| python_json_separated(
                    item,
                    item_separator,
                    key_separator,
                    ensure_ascii
                ))
                .collect::<Vec<_>>()
                .join(item_separator)
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}{key_separator}{}",
                        python_quote(key, ensure_ascii),
                        python_json_separated(
                            &values[key],
                            item_separator,
                            key_separator,
                            ensure_ascii,
                        )
                    ))
                    .collect::<Vec<_>>()
                    .join(item_separator)
            )
        }
    }
}

fn python_quote(value: &str, ensure_ascii: bool) -> String {
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
