//! The account order of a project: which accounts new chats and workers prefer.
//!
//! An account row can carry one group name (`group`). A project's order is a list of
//! group names (`accountGroups`); the account registry keeps the default order
//! (`defaultAccountGroups`) for projects without their own order. The order only sorts
//! candidates. It never removes an account: an explicit choice and the next available
//! account always stay possible. The cross-language decision cases are in
//! `workspaces/client/contracts/account-preference.json`; the Python server and the web
//! client run the same cases.

use serde_json::Value;
use std::sync::LazyLock;
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::shared_values::{
    ACCOUNT_GROUP_PATTERN, ACCOUNT_GROUPS_MAX, DEFAULT_CLAUDE_LEAD_EFFORT,
    DEFAULT_CLAUDE_LEAD_MODEL,
};

/// Group name field of an account registry row and of its public projection.
pub const ACCOUNT_GROUP_FIELD: &str = "group";
/// Default order field of the account registry and of `GET /api/accounts`.
pub const DEFAULT_GROUPS_FIELD: &str = "defaultAccountGroups";
/// Account order field of a project record.
pub const PROJECT_GROUPS_FIELD: &str = "accountGroups";
/// Revision of a project's account order (unit: saved changes).
pub const PROJECT_GROUPS_REVISION_FIELD: &str = "accountGroupsRevision";

static GROUP_PATTERN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(ACCOUNT_GROUP_PATTERN).expect("the group pattern compiles"));

/// Whether `name` is a valid account group name.
#[must_use]
pub fn valid_group(name: &str) -> bool {
    GROUP_PATTERN.is_match(name)
}

/// Validate an account order: valid names, no repeats, at most `ACCOUNT_GROUPS_MAX`.
pub fn validate_groups(groups: &[String]) -> Result<(), &'static str> {
    if groups.len() > ACCOUNT_GROUPS_MAX {
        return Err("An account order has too many groups");
    }
    if !groups.iter().all(|group| valid_group(group)) {
        return Err("A group name has 1 to 32 lowercase letters, digits, hyphens or underscores");
    }
    let mut seen = std::collections::HashSet::new();
    if !groups.iter().all(|group| seen.insert(group.as_str())) {
        return Err("Each group can be in an account order once");
    }
    Ok(())
}

/// The group names of a stored list; other values are ignored.
#[must_use]
pub fn groups_of(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The order that applies to `project`: its own non-empty order, else the default.
#[must_use]
pub fn effective_groups(project: Option<&Value>, default_groups: &[String]) -> Vec<String> {
    let own = groups_of(project.and_then(|row| row.get(PROJECT_GROUPS_FIELD)));
    if own.is_empty() {
        default_groups.to_vec()
    } else {
        own
    }
}

/// Position of `group` in the order; a missing or unlisted group sorts last.
#[must_use]
pub fn group_rank(group: Option<&str>, groups: &[String]) -> usize {
    group
        .and_then(|group| groups.iter().position(|item| item == group))
        .unwrap_or(groups.len())
}

fn account_group(account: &Value) -> Option<&str> {
    account.get(ACCOUNT_GROUP_FIELD).and_then(Value::as_str)
}

fn flag(account: &Value, field: &str) -> bool {
    account.get(field).and_then(Value::as_bool) == Some(true)
}

fn provider(account: &Value) -> &'static str {
    ProviderKind::key_of(account).unwrap_or(ProviderKind::Codex.key())
}

/// Whether a new chat can start on this public account row.
#[must_use]
pub fn can_start(account: &Value) -> bool {
    !flag(account, "disconnected")
        && !flag(account, "deleted")
        && account
            .get("duplicateOf")
            .is_none_or(|value| value.is_null() || value.as_str() == Some(""))
        && (account.get("status").and_then(Value::as_str) == Some("ready")
            || flag(account, "canAttemptNativeProof"))
}

/// The default account, model and effort of a new chat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewChatDefault {
    pub account_key: String,
    /// Set only when the request names no model and the account is a Claude account.
    pub model: Option<&'static str>,
    /// Set together with `model`.
    pub effort: Option<&'static str>,
}

/// Choose the default account of a new chat whose request names no account.
/// `accounts` is the public account list in listing order; `project` is the
/// project record of the chat's folder, if any; `model` is the requested model.
#[must_use]
pub fn new_chat_default(
    accounts: &[Value],
    default_account_key: &str,
    default_groups: &[String],
    project: Option<&Value>,
    model: Option<&str>,
) -> NewChatDefault {
    let groups = effective_groups(project, default_groups);
    let project_account = project
        .and_then(|row| row.get("accountKey"))
        .and_then(Value::as_str);
    let allowed = groups_of(project.and_then(|row| row.get("accountKeys")));
    let mut candidates = accounts
        .iter()
        .enumerate()
        .filter(|(_, row)| can_start(row))
        .filter_map(|(index, row)| Some((index, row.get("id")?.as_str()?, row)))
        .filter(|(_, key, _)| allowed.is_empty() || allowed.iter().any(|item| item == key))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(index, key, row)| {
        (
            group_rank(account_group(row), &groups),
            Some(*key) != project_account,
            *key != default_account_key,
            *index,
        )
    });
    let wanted = match model.filter(|model| !model.is_empty()) {
        Some(model) => crate::provider_adapter::model_provider(model).map(ProviderKind::key),
        None => candidates
            .iter()
            .any(|(_, _, row)| provider(row) == ProviderKind::Claude.key())
            .then_some(ProviderKind::Claude.key()),
    };
    let chosen = candidates
        .iter()
        .find(|(_, _, row)| wanted.is_none_or(|wanted| provider(row) == wanted))
        .map(|(_, key, _)| (*key).to_owned())
        .unwrap_or_else(|| project_account.unwrap_or(default_account_key).to_owned());
    let claude = accounts.iter().any(|row| {
        row.get("id").and_then(Value::as_str) == Some(chosen.as_str())
            && provider(row) == ProviderKind::Claude.key()
    });
    let defaults = model.is_none() && claude;
    NewChatDefault {
        account_key: chosen,
        model: defaults.then_some(DEFAULT_CLAUDE_LEAD_MODEL),
        effort: defaults.then_some(DEFAULT_CLAUDE_LEAD_EFFORT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shared_decision_case_holds() {
        let contract: Value = serde_json::from_str(include_str!(
            "../../../../client/contracts/account-preference.json"
        ))
        .expect("the contract parses");
        let cases = contract["cases"].as_array().expect("cases");
        assert!(cases.len() >= 10);
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let accounts = case["accounts"].as_array().expect("accounts");
            let project = case.get("project").filter(|value| !value.is_null());
            let actual = new_chat_default(
                accounts,
                case["defaultAccountKey"].as_str().expect("default key"),
                &groups_of(case.get("defaultAccountGroups")),
                project,
                case["model"].as_str(),
            );
            let expected = &case["expected"];
            assert_eq!(
                actual.account_key,
                expected["accountKey"].as_str().expect("account"),
                "{name}"
            );
            assert_eq!(actual.model, expected["model"].as_str(), "{name}");
            assert_eq!(actual.effort, expected["effort"].as_str(), "{name}");
        }
    }

    #[test]
    fn group_orders_are_validated() {
        let names = |items: &[&str]| {
            items
                .iter()
                .map(|item| (*item).to_owned())
                .collect::<Vec<_>>()
        };
        assert!(validate_groups(&names(&["personal", "attar", "team_2", "a-b"])).is_ok());
        assert!(validate_groups(&names(&[])).is_ok());
        assert!(validate_groups(&names(&["Personal"])).is_err());
        assert!(validate_groups(&names(&["-x"])).is_err());
        assert!(validate_groups(&names(&[""])).is_err());
        assert!(validate_groups(&names(&[&"a".repeat(33)])).is_err());
        assert!(validate_groups(&names(&["a", "a"])).is_err());
        let many = (0..=ACCOUNT_GROUPS_MAX)
            .map(|index| format!("g{index}"))
            .collect::<Vec<_>>();
        assert!(validate_groups(&many).is_err());
    }

    #[test]
    fn unlisted_and_missing_groups_rank_last() {
        let order = vec!["own".to_owned(), "work".to_owned()];
        assert_eq!(group_rank(Some("own"), &order), 0);
        assert_eq!(group_rank(Some("work"), &order), 1);
        assert_eq!(group_rank(Some("other"), &order), 2);
        assert_eq!(group_rank(None, &order), 2);
    }
}
