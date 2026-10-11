//! Canonical JSON of the vault: the bytes of Python's
//! `json.dumps(value, sort_keys=True, separators=(",", ":"))`. A delta tree takes the previous
//! canonical tree as its zstd prefix, so the Python writer and this one must write equal bytes
//! for an equal tree.

use anyhow::{bail, Result};
use serde_json::Value;

/// `value` as canonical JSON: object keys sorted by code point, no spaces, and every character
/// outside printable ASCII escaped as Python's `ensure_ascii` does it.
pub fn canonical(value: &Value) -> Result<Vec<u8>> {
    let mut out = String::new();
    write(value, &mut out)?;
    Ok(out.into_bytes())
}

fn write(value: &Value, out: &mut String) -> Result<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if !(n.is_i64() || n.is_u64()) {
                bail!("the vault format has no fractional numbers: {n}");
            }
            out.push_str(&n.to_string());
        }
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // UTF-8 byte order is code point order, which Python sorts by; sort here so that
            // the order does not depend on the map type that serde_json is built with.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(key, out);
                out.push(':');
                write(&map[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// A JSON string as Python writes it with `ensure_ascii`: printable ASCII stays, `"` and `\`
/// and the five short escapes are kept short, everything else is `\uXXXX` in lowercase hex,
/// with a surrogate pair above U+FFFF.
pub fn string(text: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    write!(out, "\\u{unit:04x}").expect("a String takes any text");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn equals_python_json_dumps() {
        // python3: json.dumps(v, sort_keys=True, separators=(",", ":"))
        let value = json!({
            "files": {
                "caf\u{e9}/\u{4e2d}\u{6587}.txt": ["100644", 3, "ab", [[1, "0a1b2c3d", 0, 0, 12, "full"]]],
                "tab\there\nnew\u{1}\u{1f}\u{7f}\"q\"\\b/s\u{8}\u{c}\r": ["120000", 0, "cd", []],
                "emoji \u{1F600} \u{10000} \u{10FFFF} \u{2028} \u{feff}": ["100755", u64::MAX, "", []],
                "Z": [], "a": {}, "\u{e9}": null, "\u{e000}": true, "\u{1F600}x": false
            },
            "base": null, "v": 1, "neg": -5
        });
        let python = r#"{"base":null,"files":{"Z":[],"a":{},"caf\u00e9/\u4e2d\u6587.txt":["100644",3,"ab",[[1,"0a1b2c3d",0,0,12,"full"]]],"emoji \ud83d\ude00 \ud800\udc00 \udbff\udfff \u2028 \ufeff":["100755",18446744073709551615,"",[]],"tab\there\nnew\u0001\u001f\u007f\"q\"\\b/s\b\f\r":["120000",0,"cd",[]],"\u00e9":null,"\ue000":true,"\ud83d\ude00x":false},"neg":-5,"v":1}"#;
        assert_eq!(
            String::from_utf8(canonical(&value).unwrap()).unwrap(),
            python
        );
        // The bytes read back as the same value.
        let back: Value = serde_json::from_str(python).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn fractions_are_refused() {
        assert!(canonical(&json!({"time": 1.5})).is_err());
    }
}
