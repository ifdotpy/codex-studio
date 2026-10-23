use serde_json::Value;

/// Compare complete documents, allowing numeric equivalence only for the
/// `rate` and `outputTokens` members of TokenRateRecord entries in the named
/// token-rate snapshot. Every other field remains exact.
pub fn assert_token_rate_documents_equal(
    expected: &Value,
    actual: &Value,
    snapshot_path: &[&str],
    context: &str,
) {
    let mut expected = expected.clone();
    let mut actual = actual.clone();
    let expected_snapshot = at_path_mut(&mut expected, snapshot_path);
    let actual_snapshot = at_path_mut(&mut actual, snapshot_path);
    if expected_snapshot.get("records").is_none() && actual_snapshot.get("records").is_none() {
        assert_eq!(expected, actual, "{context}");
        return;
    }
    let expected_records = expected_snapshot["records"]
        .as_array_mut()
        .expect("expected records");
    let actual_records = actual_snapshot["records"]
        .as_array_mut()
        .expect("actual records");
    assert_eq!(
        expected_records.len(),
        actual_records.len(),
        "{context}: record count"
    );
    for (expected, actual) in expected_records.iter_mut().zip(actual_records) {
        compare_token_rate(expected, actual, context, "records");
    }
    assert_eq!(
        expected, actual,
        "{context}: non-token-rate fields must match exactly"
    );
}

fn compare_token_rate(expected: &mut Value, actual: &mut Value, context: &str, path: &str) {
    for field in ["rate", "outputTokens"] {
        let expected_number = expected[field]
            .as_f64()
            .unwrap_or_else(|| panic!("{context}: expected {path}.{field} number"));
        let actual_number = actual[field]
            .as_f64()
            .unwrap_or_else(|| panic!("{context}: actual {path}.{field} number"));
        assert_eq!(
            expected_number, actual_number,
            "{context}: token-rate numeric field {path}.{field}"
        );
        expected[field] = Value::String("<token-rate-number>".into());
        actual[field] = Value::String("<token-rate-number>".into());
    }
}

fn at_path_mut<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut Value {
    path.iter().fold(value, |current, key| &mut current[*key])
}
