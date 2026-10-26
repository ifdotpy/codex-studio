use serde_json::Value;

/// Compare complete documents, allowing numeric equivalence only for the
/// `rate` and `outputTokens` members of TokenRateValue entries in the named
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
    let expected_has_groups =
        expected_snapshot.get("rates").is_some() || expected_snapshot.get("teams").is_some();
    let actual_has_groups =
        actual_snapshot.get("rates").is_some() || actual_snapshot.get("teams").is_some();
    if !expected_has_groups && !actual_has_groups {
        assert_eq!(expected, actual, "{context}");
        return;
    }
    assert!(
        expected_has_groups && actual_has_groups,
        "{context}: token-rate snapshot shape differs"
    );
    for group in ["rates", "teams"] {
        let expected_group = expected_snapshot[group]
            .as_object_mut()
            .unwrap_or_else(|| panic!("{context}: expected token-rate {group} object"));
        let actual_group = actual_snapshot[group]
            .as_object_mut()
            .unwrap_or_else(|| panic!("{context}: actual token-rate {group} object"));
        assert_eq!(
            expected_group.keys().collect::<Vec<_>>(),
            actual_group.keys().collect::<Vec<_>>(),
            "{context}: token-rate {group} keys"
        );
        for (key, expected_value) in expected_group {
            let actual_value = actual_group
                .get_mut(key)
                .unwrap_or_else(|| panic!("{context}: missing token-rate {group}.{key}"));
            if group == "teams" {
                let expected_agents = expected_value
                    .as_object_mut()
                    .unwrap_or_else(|| panic!("{context}: expected token-rate team {key} object"));
                let actual_agents = actual_value
                    .as_object_mut()
                    .unwrap_or_else(|| panic!("{context}: actual token-rate team {key} object"));
                assert_eq!(
                    expected_agents.keys().collect::<Vec<_>>(),
                    actual_agents.keys().collect::<Vec<_>>(),
                    "{context}: token-rate team {key} agent keys"
                );
                for (agent, expected_rate) in expected_agents {
                    let actual_rate = actual_agents.get_mut(agent).unwrap_or_else(|| {
                        panic!("{context}: missing token-rate team {key}.{agent}")
                    });
                    compare_token_rate(
                        expected_rate,
                        actual_rate,
                        context,
                        &format!("teams.{key}.{agent}"),
                    );
                }
            } else {
                compare_token_rate(
                    expected_value,
                    actual_value,
                    context,
                    &format!("rates.{key}"),
                );
            }
        }
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
