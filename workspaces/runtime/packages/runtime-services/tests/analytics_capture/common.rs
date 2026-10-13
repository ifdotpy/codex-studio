use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::{Value, json};
pub fn actual(db: &Connection, expected: &Value) -> Value {
    let mut result = json!({});
    for (table, _) in expected.as_object().expect("tables") {
        let mut stmt = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("read");
        let cols = stmt
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let values = stmt
            .query_map([], |r| {
                let mut v = json!({});
                for (i, c) in cols.iter().enumerate() {
                    v[c] = match r.get::<_, SqlValue>(i)? {
                        SqlValue::Null => Value::Null,
                        SqlValue::Text(s) => json!(s),
                        SqlValue::Integer(n) => json!(n),
                        SqlValue::Real(n) => json!(n),
                        SqlValue::Blob(_) => panic!("not blob"),
                    }
                }
                Ok(v)
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("all rows");
        result[table] = json!(values);
    }
    result
}
pub fn compare(actual: &Value, expected: &Value, path: &str) {
    compare_with_fingerprint(actual, expected, path, None)
}
/// The only registered representation deviation is a new Claude content hash.
/// All other fields, including unchanged legacy fingerprints, compare exactly.
pub fn compare_with_fingerprint(
    actual: &Value,
    expected: &Value,
    path: &str,
    canonical: Option<(&str, &Value)>,
) {
    if let Some((allowed_path, value)) = canonical
        && path == allowed_path
        && actual != expected
    {
        assert_eq!(actual, value, "canonical Claude fingerprint {path}");
        return;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(
                a.keys().collect::<Vec<_>>(),
                b.keys().collect::<Vec<_>>(),
                "keys {path}"
            );
            for k in b.keys() {
                compare_with_fingerprint(&a[k], &b[k], &format!("{path}/{k}"), canonical);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "length {path}");
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                compare_with_fingerprint(a, b, &format!("{path}/{i}"), canonical);
            }
        }
        (Value::Number(a), Value::Number(b)) => assert_eq!(a.as_f64(), b.as_f64(), "number {path}"),
        (Value::String(a), Value::String(b))
            if path.ends_with("/record") || path.ends_with("/value") =>
        {
            match (
                serde_json::from_str::<Value>(a),
                serde_json::from_str::<Value>(b),
            ) {
                (Ok(a), Ok(b)) => compare_with_fingerprint(&a, &b, path, canonical),
                _ => assert_eq!(actual, expected, "{path}"),
            }
        }
        _ => assert_eq!(actual, expected, "{path}"),
    }
}
