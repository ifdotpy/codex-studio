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

pub(crate) fn belongs_to(identity: &str, email: &str) -> bool {
    identity == format!("claude:{email}")
        || tuple(identity).is_some_and(|(stored, _, _)| stored == email)
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
        std::fs::write(&path, json!({"oauthAccount":{"emailAddress":"a@example.test","organizationUuid":"org-a","accountUuid":"account-a"}}).to_string()).expect("native profile writes");
        let observed = with_native_identity(metadata.clone(), &path);
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
