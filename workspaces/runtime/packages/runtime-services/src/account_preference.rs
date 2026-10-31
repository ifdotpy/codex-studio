//! The account order of a project: which accounts new chats and workers prefer.
//!
//! An account row can carry one group name (`group`). A project's order is a list of
//! group names (`accountGroups`); the account registry keeps the default order
//! (`defaultAccountGroups`) for projects without their own order. The order only sorts
//! candidates. It never removes an account: an explicit choice and the next available
//! account always stay possible. The cross-language decision cases are in
//! `workspaces/client/contracts/account-preference.json`; the Python server and the web
//! client run the same cases.

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::sync::LazyLock;
use studio_runtime_schema::ProviderKind;
use studio_runtime_schema::project_record::ProjectAccountPreference;
use studio_runtime_schema::shared_values::{
    ACCOUNT_GROUP_PATTERN, ACCOUNT_GROUPS_MAX, DEFAULT_CLAUDE_LEAD_EFFORT,
    DEFAULT_CLAUDE_LEAD_MODEL,
};

/// File beside `accounts/registry.json` that holds the account groups and the
/// default order. It is a separate file so that an older build, whose registry
/// schema rejects unknown fields, still reads the registry after a rollback.
pub const ACCOUNT_ORDER_FILE: &str = "account-order.json";
/// Version of [`ACCOUNT_ORDER_FILE`].
const ACCOUNT_ORDER_VERSION: u64 = 1;

/// Account groups by account key, and the default account order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountOrder {
    pub groups: std::collections::BTreeMap<String, String>,
    pub default_groups: Vec<String>,
}

// The file is a preference: malformed containers and non-string entries are
// ignored independently, while an unsupported version discards the whole file.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountOrderFile {
    version: u64,
    #[serde(default)]
    groups: Option<GroupMap>,
    #[serde(default)]
    default_account_groups: Option<GroupList>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupMap {
    Map(std::collections::BTreeMap<String, GroupValue>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupList {
    List(Vec<GroupValue>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupValue {
    Name(String),
    Other(serde::de::IgnoredAny),
}

impl GroupValue {
    fn name(self) -> Option<String> {
        match self {
            Self::Name(name) => Some(name),
            Self::Other(_) => None,
        }
    }
}

// Serde's struct derive also accepts positional arrays. The file is an object.
fn read_order_file<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<AccountOrderFile, D::Error> {
    struct Object;
    impl<'de> serde::de::Visitor<'de> for Object {
        type Value = AccountOrderFile;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an account order object")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            AccountOrderFile::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(Object)
}

impl AccountOrder {
    /// Read the file at `path`. A missing or unreadable file, another version, and
    /// invalid names give no order: the order is a preference, and account reads
    /// must not fail because of it.
    #[must_use]
    pub fn read(path: &std::path::Path) -> Self {
        let Some(file) = std::fs::read(path)
            .ok()
            .and_then(|bytes| {
                let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
                let file = read_order_file(&mut deserializer).ok()?;
                deserializer.end().ok()?;
                Some(file)
            })
            .filter(|file| file.version == ACCOUNT_ORDER_VERSION)
        else {
            return Self::default();
        };
        let groups = match file.groups {
            Some(GroupMap::Map(rows)) => rows
                .into_iter()
                .filter_map(|(key, group)| Some((key, group.name()?)))
                .filter(|(_, group)| valid_group(group))
                .collect(),
            Some(GroupMap::Other(_)) | None => Default::default(),
        };
        let default_groups = match file.default_account_groups {
            Some(GroupList::List(items)) => {
                items.into_iter().filter_map(GroupValue::name).collect()
            }
            Some(GroupList::Other(_)) | None => Vec::new(),
        };
        Self {
            groups,
            default_groups: if validate_groups(&default_groups).is_ok() {
                default_groups
            } else {
                Vec::new()
            },
        }
    }

    /// The file content.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": ACCOUNT_ORDER_VERSION,
            "groups": self.groups,
            DEFAULT_GROUPS_FIELD: self.default_groups,
        }))
        .unwrap_or_default()
    }

    /// Add each account's group and the default order to a public snapshot.
    pub fn apply(&self, snapshot: &mut crate::account_policy::AccountsSnapshot) {
        for row in snapshot
            .accounts
            .iter_mut()
            .chain(snapshot.archived_accounts.iter_mut())
        {
            if let Some(group) = row.id.as_str().and_then(|id| self.groups.get(id)) {
                row.group = crate::account_policy::PublicString::String(group.clone());
            }
        }
        snapshot.default_account_groups = self.default_groups.clone();
    }
}

/// Group name field of the public account projection.
pub const ACCOUNT_GROUP_FIELD: &str = "group";
/// Default order field of the account order file and of `GET /api/accounts`.
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
pub fn effective_groups(
    project: Option<&ProjectAccountPreference>,
    default_groups: &[String],
) -> Vec<String> {
    project
        .map(|row| &row.account_groups)
        .filter(|groups| !groups.is_empty())
        .cloned()
        .unwrap_or_else(|| default_groups.to_vec())
}

/// Position of `group` in the order; a missing or unlisted group sorts last.
#[must_use]
pub fn group_rank(group: Option<&str>, groups: &[String]) -> usize {
    group
        .and_then(|group| groups.iter().position(|item| item == group))
        .unwrap_or(groups.len())
}

/// Whether a new chat can start on this public account row.
#[must_use]
pub fn can_start(account: &crate::account_policy::PublicAccount) -> bool {
    account.provider_kind().is_some()
        && account.disconnected.as_bool() != Some(true)
        && account.deleted.as_bool() != Some(true)
        && (!account.duplicate_of.is_present_non_null()
            || account.duplicate_of.as_str() == Some(""))
        && (account.status.as_str() == Some("ready")
            || account.can_attempt_native_proof.as_bool() == Some(true))
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
    accounts: &[crate::account_policy::PublicAccount],
    default_account_key: &str,
    default_groups: &[String],
    project: Option<&ProjectAccountPreference>,
    model: Option<&str>,
) -> NewChatDefault {
    let groups = effective_groups(project, default_groups);
    let project_account = project.and_then(|row| row.account_key.as_deref());
    let allowed = project
        .map(|row| row.account_keys.as_slice())
        .unwrap_or_default();
    let mut candidates = accounts
        .iter()
        .enumerate()
        .filter(|(_, row)| can_start(row))
        .filter_map(|(index, row)| Some((index, row.id.as_str()?, row)))
        .filter(|(_, key, _)| allowed.is_empty() || allowed.iter().any(|item| item == key))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(index, key, row)| {
        (
            group_rank(row.group.as_str(), &groups),
            Some(*key) != project_account,
            *key != default_account_key,
            *index,
        )
    });
    let wanted = match model.filter(|model| !model.is_empty()) {
        Some(model) => crate::provider_adapter::model_provider(model),
        None => candidates
            .iter()
            .any(|(_, _, row)| row.provider_kind() == Some(ProviderKind::Claude))
            .then_some(ProviderKind::Claude),
    };
    let chosen = candidates
        .iter()
        .find(|(_, _, row)| wanted.is_none_or(|wanted| row.provider_kind() == Some(wanted)))
        .map(|(_, key, _)| (*key).to_owned())
        .unwrap_or_else(|| project_account.unwrap_or(default_account_key).to_owned());
    let claude = accounts.iter().any(|row| {
        row.id.as_str() == Some(chosen.as_str())
            && row.provider_kind() == Some(ProviderKind::Claude)
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
    fn account_order_file_filters_entries_and_preserves_fallbacks() {
        let dir = tempfile::tempdir().expect("temporary order directory");
        let path = dir.path().join(ACCOUNT_ORDER_FILE);
        let read = |value: Value| {
            std::fs::write(&path, serde_json::to_vec(&value).expect("encode fixture"))
                .expect("write fixture");
            AccountOrder::read(&path)
        };
        let order = read(serde_json::json!({
            "version": 1,
            "groups": {"a":"team", "b":false, "c":"Bad", "d":"work"},
            "defaultAccountGroups": ["work", 2, null, "team"],
            "future": true
        }));
        assert_eq!(
            order.groups,
            [("a".into(), "team".into()), ("d".into(), "work".into())].into()
        );
        assert_eq!(order.default_groups, ["work", "team"]);
        std::fs::write(&path, order.encode()).expect("write order");
        assert_eq!(AccountOrder::read(&path), order);
        for value in [
            serde_json::json!([]),
            serde_json::json!([1, {}, []]),
            serde_json::json!({"version": null}),
            serde_json::json!({"version": 1.0}),
            serde_json::json!({"version": 2, "groups":{"a":"team"}}),
        ] {
            assert_eq!(read(value), AccountOrder::default());
        }
        let order = read(serde_json::json!({"version":1,"groups":[],"defaultAccountGroups":{}}));
        assert_eq!(order, AccountOrder::default());
        let order = read(
            serde_json::json!({"version":1,"groups":{"a":"team"},"defaultAccountGroups":["same","same"]}),
        );
        assert_eq!(order.groups.get("a").map(String::as_str), Some("team"));
        assert!(order.default_groups.is_empty());
    }

    #[test]
    fn provider_identity_unknown_accounts_cannot_be_selected() {
        use serde_json::json;
        for provider in [
            None,
            Some(Value::Null),
            Some(json!("future")),
            Some(json!(7)),
        ] {
            let mut unknown = json!({"id":"unknown","status":"ready"});
            if let Some(provider) = provider {
                unknown["provider"] = provider;
            }
            let unknown: crate::account_policy::PublicAccount =
                serde::Deserialize::deserialize(unknown).expect("fixture");
            assert!(!can_start(&unknown));
            let accounts = [
                unknown,
                <crate::account_policy::PublicAccount as serde::Deserialize>::deserialize(
                    json!({"id":"known","provider":"codex","status":"ready"}),
                )
                .expect("fixture"),
            ];
            assert_eq!(
                new_chat_default(&accounts, "unknown", &[], None, Some("gpt-mini")).account_key,
                "known"
            );
        }
        for provider in ProviderKind::ALL {
            assert!(can_start(
                &<crate::account_policy::PublicAccount as serde::Deserialize>::deserialize(
                    json!({"provider":provider.key(),"status":"ready"})
                )
                .expect("fixture")
            ));
        }
    }

    #[test]
    fn every_shared_decision_case_holds() {
        #[derive(Deserialize)]
        struct Contract {
            cases: Vec<Case>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Case {
            name: String,
            accounts: Vec<crate::account_policy::PublicAccount>,
            default_account_key: String,
            default_account_groups: Vec<String>,
            project: Option<ProjectAccountPreference>,
            model: Option<String>,
            expected: Expected,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Expected {
            account_key: String,
            model: Option<String>,
            effort: Option<String>,
        }
        let contract: Contract = serde_json::from_str(include_str!(
            "../../../../client/contracts/account-preference.json"
        ))
        .expect("the contract parses");
        let cases = contract.cases;
        assert!(cases.len() >= 10);
        for case in cases {
            let actual = new_chat_default(
                &case.accounts,
                &case.default_account_key,
                &case.default_account_groups,
                case.project.as_ref(),
                case.model.as_deref(),
            );
            let expected = &case.expected;
            assert_eq!(actual.account_key, expected.account_key, "{}", case.name);
            assert_eq!(actual.model, expected.model.as_deref(), "{}", case.name);
            assert_eq!(actual.effort, expected.effort.as_deref(), "{}", case.name);
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
