//! Claude credential identity. Account IDs remain email based for compatibility.
//! The versioned tuple is [email, organization UUID, account UUID]. Missing fields
//! can be learned once; a learned field can never be removed or replaced.
use super::{AuthMetadata, AuthStatus};
use serde::Deserialize;
use std::{fs::File, io::Read, path::Path};

const PREFIX: &str = "claude:v2:";

pub(crate) fn identity(email: &str, organization: Option<&str>, account: Option<&str>) -> String {
    let organization = organization.filter(|value| !value.trim().is_empty());
    let account = account.filter(|value| !value.trim().is_empty());
    if organization.is_none() && account.is_none() {
        return format!("claude:{email}");
    }
    format!(
        "{PREFIX}{}",
        serde_json::json!([email, organization, account])
    )
}

fn tuple(value: &str) -> Option<(String, Option<String>, Option<String>)> {
    let tuple: (String, Option<String>, Option<String>) =
        serde_json::from_str(value.strip_prefix(PREFIX)?).ok()?;
    (!tuple.0.is_empty()
        && tuple.1.as_ref().is_none_or(|v| !v.trim().is_empty())
        && tuple.2.as_ref().is_none_or(|v| !v.trim().is_empty()))
    .then_some(tuple)
}

/// Directional comparison: a legacy or partial pin can gain evidence only.
pub(crate) fn accepts(expected: &str, observed: &str) -> bool {
    if expected == observed {
        return true;
    }
    let Some((email, organization, account)) = tuple(observed) else {
        return false;
    };
    if expected == format!("claude:{email}") {
        return true;
    }
    tuple(expected).is_some_and(|(old_email, old_org, old_account)| {
        old_email == email
            && old_org
                .as_ref()
                .is_none_or(|old| Some(old) == organization.as_ref())
            && old_account
                .as_ref()
                .is_none_or(|old| Some(old) == account.as_ref())
    })
}

/// The pins that `current` can have grown from: the legacy form and the
/// tuples with fewer learned fields. A bridge launched with one of them holds
/// the same credential, and `accepts` decides each candidate.
pub(crate) fn earlier_forms(current: &str) -> Vec<String> {
    let Some((email, organization, account)) = tuple(current) else {
        return Vec::new();
    };
    let organizations = [None, organization.as_deref()];
    let accounts = [None, account.as_deref()];
    let mut forms = vec![format!("claude:{email}")];
    for organization in organizations {
        for account in accounts {
            forms.push(format!(
                "{PREFIX}{}",
                serde_json::json!([email, organization, account])
            ));
        }
    }
    forms.sort();
    forms.dedup();
    forms.retain(|form| form != current && accepts(form, current));
    forms
}

pub(crate) fn belongs_to(identity: &str, email: &str) -> bool {
    identity == format!("claude:{email}")
        || tuple(identity).is_some_and(|(stored, _, _)| stored == email)
}

/// Only this suffix, never the credential tuple or full UUID, reaches the UI.
const ORGANIZATION_ID_SUFFIX_CHARS: usize = 8;
pub(crate) fn organization(
    identity: Option<&str>,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    let (_, org, _) = tuple(identity?)?;
    let org = org?;
    let suffix: String = org
        .chars()
        .rev()
        .take(ORGANIZATION_ID_SUFFIX_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(serde_json::json!({"name": name, "idSuffix": suffix}))
}

pub(crate) fn organization_mismatch(
    expected: Option<&str>,
    expected_name: Option<&str>,
    observed: Option<&str>,
    observed_name: Option<&str>,
) -> Option<serde_json::Value> {
    let (_, old_org, _) = tuple(expected?)?;
    let (_, new_org, _) = tuple(observed?)?;
    if old_org.is_none() || new_org.is_none() || old_org == new_org {
        return None;
    }
    Some(serde_json::json!({
        "expected": organization(expected, expected_name)?,
        "observed": organization(observed, observed_name)?,
    }))
}

/// Native auth status omits the account UUID in some CLI versions. Read only
/// the native profile metadata and require agreement with the status result.
pub(crate) fn with_native_identity(mut metadata: AuthMetadata, path: &Path) -> AuthMetadata {
    if metadata.status != AuthStatus::Ready {
        return metadata;
    }
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "oauthAccount")]
        account: Option<Account>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Account {
        email_address: Option<String>,
        organization_uuid: Option<String>,
        organization_name: Option<String>,
        account_uuid: Option<String>,
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return metadata,
        Err(_) => return AuthMetadata::error(Some("parser")),
    };
    let config: Config =
        match serde_json::from_reader(file.take(super::auth_cache::CREDENTIAL_READ_LIMIT_BYTES)) {
            Ok(config) => config,
            Err(_) => return AuthMetadata::error(Some("parser")),
        };
    let Some(account) = config.account else {
        return metadata;
    };
    if account.email_address != metadata.email {
        return AuthMetadata::error(Some("parser"));
    }
    let Some(email) = metadata.email.as_deref() else {
        return metadata;
    };
    let Some(previous) = metadata.credential_identity.as_deref() else {
        return metadata;
    };
    let (_, previous_org, previous_account) =
        tuple(previous).unwrap_or_else(|| (email.to_owned(), None, None));
    if previous_org
        .as_ref()
        .zip(account.organization_uuid.as_ref())
        .is_some_and(|(a, b)| a != b)
        || previous_account
            .as_ref()
            .zip(account.account_uuid.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        return AuthMetadata::error(Some("parser"));
    }
    if metadata.organization_name.is_none() {
        metadata.organization_name = account
            .organization_name
            .filter(|name| !name.trim().is_empty());
    }
    metadata.credential_identity = Some(identity(
        email,
        previous_org
            .as_deref()
            .or(account.organization_uuid.as_deref()),
        previous_account
            .as_deref()
            .or(account.account_uuid.as_deref()),
    ));
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn shared_claude_identity_contract() {
        let cases: Value = serde_json::from_str(include_str!("claude_identity_cases.json"))
            .expect("identity cases parse");
        let identities = cases["identities"]
            .as_array()
            .expect("identity cases are an array");
        for case in identities {
            assert_eq!(
                identity(
                    case[0].as_str().expect("identity email is text"),
                    case[1].as_str(),
                    case[2].as_str()
                ),
                case[3].as_str().expect("expected identity is text")
            );
        }
        for case in cases["comparisons"]
            .as_array()
            .expect("comparison cases are an array")
        {
            let old = identities
                [case[0].as_u64().expect("old identity index is unsigned") as usize][3]
                .as_str()
                .expect("old identity is text");
            let new = identities
                [case[1].as_u64().expect("new identity index is unsigned") as usize][3]
                .as_str()
                .expect("new identity is text");
            assert_eq!(
                accepts(old, new),
                case[2].as_bool().expect("expected acceptance is boolean"),
                "{old} -> {new}"
            );
        }
    }

    #[test]
    fn native_profile_adds_uuid_and_refuses_conflicting_organization() {
        let dir = tempfile::tempdir().expect("temporary home creates");
        let path = dir.path().join(".claude.json");
        let status = br#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@example.test","orgId":"org-a"}"#;
        let metadata = super::super::parse_claude_status(status, true);
        std::fs::write(&path, json!({"oauthAccount":{"emailAddress":"a@example.test","organizationUuid":"org-a","organizationName":"Fixture organization","accountUuid":"account-a"}}).to_string()).expect("native profile writes");
        let observed = with_native_identity(metadata.clone(), &path);
        assert_eq!(
            observed.organization_name.as_deref(),
            Some("Fixture organization")
        );
        assert_eq!(
            observed.credential_identity.as_deref(),
            Some(identity("a@example.test", Some("org-a"), Some("account-a")).as_str())
        );
        std::fs::write(&path, json!({"oauthAccount":{"emailAddress":"a@example.test","organizationUuid":"org-b","accountUuid":"account-a"}}).to_string()).expect("conflicting native profile writes");
        assert_eq!(
            with_native_identity(metadata, &path).status,
            AuthStatus::Error
        );
    }
}
