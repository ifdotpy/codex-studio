//! Account registry v1 -> current. Original account/login order is preserved.
//! Atomic replacement can succeed while directory durability remains unknown.
use super::{MigrationDefinition, OCTOBER_26, REMOVE_AFTER_SOURCE_RETIRED, failure};
use crate::{
    StoreError, StoreResult,
    account_registry::{
        RegistryOrder, ReplaceError, atomic_replace, decode_registry, encode_registry,
    },
};
use serde::Deserialize;
use std::{io::Read, path::PathBuf};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};

pub(super) const DEFINITION: MigrationDefinition = MigrationDefinition {
    id: "2026-10-26-account-registry",
    introduced: OCTOBER_26,
    target: MigrationTarget::AccountRegistry,
    supported_source: "Registry version 1 with omitted/null providers; version 2 is strictly validated",
    remove_when: REMOVE_AFTER_SOURCE_RETIRED,
};

pub(super) struct PreparedRegistry {
    path: PathBuf,
    original: Vec<u8>,
    replacement: Option<Vec<u8>>,
    pub(super) accounts: super::Accounts,
}

fn invalid() -> StoreError {
    StoreError::RecordContract("invalid account registry")
}

pub(super) fn preflight(root: &std::path::Path) -> StoreResult<Option<PreparedRegistry>> {
    prepare(root).map_err(|source| {
        failure(
            DEFINITION.id,
            DEFINITION.target,
            MigrationPhase::Validate,
            MigrationRollbackOutcome::NotRequired,
            source,
        )
    })
}

fn prepare(root: &std::path::Path) -> StoreResult<Option<PreparedRegistry>> {
    let path = root.join("accounts/registry.json");
    let canonical = match crate::path_identity::CanonicalPath::existing(&path) {
        Ok(path) if path.starts_with(root) => path,
        Ok(_) => return Err(invalid()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut original = Vec::new();
    std::fs::File::open(&canonical)?
        .take((crate::account_state::MAX_PERSISTED_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut original)?;
    if !crate::account_state::persisted_document_within_bounds(&original)
        || original.len() > crate::account_state::MAX_PERSISTED_DOCUMENT_BYTES
    {
        return Err(invalid());
    }
    let mut value = serde_json::from_slice(&original)?;
    let changed = super::m2026_10_26_provider_identity::migrate_registry_2026_10_26(&mut value)?;
    let order = RegistryOrder::deserialize(&mut serde_json::Deserializer::from_slice(&original))
        .map_err(|_| invalid())?;
    let accounts = value["accounts"]
        .as_object()
        .ok_or_else(invalid)?
        .iter()
        .map(|(key, account)| {
            let provider =
                studio_runtime_schema::ProviderKind::of_record(account).map_err(|_| invalid())?;
            Ok((key.clone(), provider))
        })
        .collect::<StoreResult<_>>()?;
    let replacement = if changed {
        let encoded = encode_registry(&value, &order).map_err(|_| invalid())?;
        decode_registry(&encoded).map_err(|_| invalid())?;
        Some(encoded)
    } else {
        decode_registry(&original).map_err(|_| invalid())?;
        None
    };
    Ok(Some(PreparedRegistry {
        path: canonical.to_path_buf(),
        original,
        replacement,
        accounts,
    }))
}

pub(super) fn apply(prepared: Option<&PreparedRegistry>) -> StoreResult<()> {
    apply_with(prepared, atomic_replace)
}

fn apply_with(
    prepared: Option<&PreparedRegistry>,
    replace: impl FnOnce(&std::path::Path, &[u8]) -> Result<(), ReplaceError>,
) -> StoreResult<()> {
    let Some(prepared) = prepared else {
        return Ok(());
    };
    // A changed source is never overwritten. Runtime ownership serializes the
    // application's writers; this check also detects intervening external edits.
    let mut current = Vec::new();
    std::fs::File::open(&prepared.path)?
        .take((crate::account_state::MAX_PERSISTED_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut current)?;
    if current != prepared.original {
        return Err(failure(
            DEFINITION.id,
            DEFINITION.target,
            MigrationPhase::Apply,
            MigrationRollbackOutcome::NotRequired,
            StoreError::RecordContract("account registry changed after migration preflight"),
        ));
    }
    let Some(replacement) = &prepared.replacement else {
        return Ok(());
    };
    replace(&prepared.path, replacement).map_err(|outcome| {
        let (phase, rollback) = match outcome {
            ReplaceError::NotReplaced => {
                (MigrationPhase::Apply, MigrationRollbackOutcome::NotRequired)
            }
            ReplaceError::ReplacedNotDurable => {
                (MigrationPhase::Commit, MigrationRollbackOutcome::Unknown)
            }
        };
        failure(
            DEFINITION.id,
            DEFINITION.target,
            phase,
            rollback,
            StoreError::RecordContract("account registry replacement failed"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_registry::atomic_replace_with;
    use std::fs;

    fn source(root: &std::path::Path) -> Vec<u8> {
        fs::create_dir(root.join("accounts")).expect("accounts");
        let bytes = br#"{"version":1,"defaultAccountKey":"default","accounts":{"z":{"id":"z","provider":"claude","home":"/fixture/z","label":"Z","source":"fixture","status":"ready"},"default":{"id":"default","home":"/fixture","label":"Fixture","source":"fixture","status":"ready"}},"logins":{}}"#.to_vec();
        fs::write(root.join("accounts/registry.json"), &bytes).expect("source");
        bytes
    }

    #[test]
    fn migration_registry_rename_failure_keeps_source_and_newer_edit_is_refused() {
        let state = tempfile::tempdir().expect("state");
        // The store root is canonical; a macOS temp folder is a symlink.
        let root = crate::path_identity::CanonicalPath::existing(state.path())
            .expect("canonical state")
            .as_path()
            .to_path_buf();
        let original = source(root.as_path());
        let path = root.as_path().join("accounts/registry.json");
        let prepared = preflight(root.as_path()).expect("preflight");
        let error = apply_with(prepared.as_ref(), |path, bytes| {
            atomic_replace_with(
                path,
                bytes,
                |_, _| Err(std::io::Error::other("injected rename")),
                |_| Ok(()),
            )
        })
        .expect_err("rename failure");
        assert!(matches!(
            error,
            StoreError::Migration {
                phase: MigrationPhase::Apply,
                rollback: MigrationRollbackOutcome::NotRequired,
                ..
            }
        ));
        assert_eq!(fs::read(&path).expect("source"), original);
        let mut newer = original.clone();
        newer.push(b' ');
        fs::write(&path, &newer).expect("intervening edit");
        assert!(apply(prepared.as_ref()).is_err());
        assert_eq!(fs::read(path).expect("newer edit"), newer);
    }

    #[test]
    fn migration_registry_directory_sync_unknown_never_restores_over_committed_bytes() {
        let state = tempfile::tempdir().expect("state");
        // The store root is canonical; a macOS temp folder is a symlink.
        let root = crate::path_identity::CanonicalPath::existing(state.path())
            .expect("canonical state")
            .as_path()
            .to_path_buf();
        let original = source(root.as_path());
        let path = root.as_path().join("accounts/registry.json");
        let prepared = preflight(root.as_path()).expect("preflight");
        let error = apply_with(prepared.as_ref(), |path, bytes| {
            atomic_replace_with(
                path,
                bytes,
                |temporary, target| fs::rename(temporary, target),
                |_| Err(std::io::Error::other("injected directory sync")),
            )
        })
        .expect_err("unknown durability");
        assert!(matches!(
            error,
            StoreError::Migration {
                phase: MigrationPhase::Commit,
                rollback: MigrationRollbackOutcome::Unknown,
                ..
            }
        ));
        let committed = fs::read(&path).expect("replaced bytes");
        assert_ne!(committed, original);
        let (_, order) = decode_registry(&committed).expect("strict current document");
        assert_eq!(order.accounts, ["z", "default"]);
        let retry = preflight(root.as_path()).expect("restart preflight");
        apply_with(retry.as_ref(), |_, _| {
            panic!("already current file must not be replaced")
        })
        .expect("safe continuation");
        assert_eq!(fs::read(path).expect("same committed bytes"), committed);
    }
}
