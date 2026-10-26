//! The one safe projection of typed store failures for HTTP, recovery and operator diagnostics.
use studio_runtime_schema::studio_errors::HttpErrorKind as InternalErrorKind;
use studio_runtime_schema::studio_errors::{Cause, JsonCategory};

/// SQLite reserves the low byte of an extended code for its primary result code.
const SQLITE_PRIMARY_CODE_MASK: i32 = 0xff;

pub fn store_error_kind(error: &studio_runtime_store::StoreError) -> InternalErrorKind {
    use InternalErrorKind as K;
    use studio_runtime_store::StoreError;
    match error {
        StoreError::StateDirectory(_) => K::StoreIo,
        StoreError::AlreadyOwned => K::StoreOwnership,
        StoreError::MissingDatabase(_) | StoreError::UnsafeStateDirectory => K::StoreConfiguration,
        StoreError::SchemaMismatch(_)
        | StoreError::SchemaObjectMismatch { .. }
        | StoreError::RecordContract(_) => K::StoreSchema,
        StoreError::InvalidSyncObject { .. }
        | StoreError::InvalidStoredRecord { .. }
        | StoreError::InvalidStoredObject { .. }
        | StoreError::InvalidSyncObjectField { .. }
        | StoreError::Json(_)
        | StoreError::SchemaValidation(_) => K::StoreData,
        StoreError::Sqlite(_)
        | StoreError::CommitOutcomeUnknown(_)
        | StoreError::CommitActorOutcomeUnknown(_)
        | StoreError::WriterActorInitialization(_)
        | StoreError::WriterActorControl(_) => K::StoreDatabase,
        StoreError::WriterPoisoned | StoreError::ReadTaskFailed => K::StoreConcurrency,
    }
}

pub fn json_cause(error: &serde_json::Error) -> Cause {
    Cause::Json {
        line: error.line(),
        column: error.column(),
        category: match error.classify() {
            serde_json::error::Category::Io => JsonCategory::Io,
            serde_json::error::Category::Syntax => JsonCategory::Syntax,
            serde_json::error::Category::Data => JsonCategory::Data,
            serde_json::error::Category::Eof => JsonCategory::Eof,
        },
    }
}

/// Extract facts while the typed source still exists. Error messages, SQL and record values stay private.
pub fn store_causes(error: &studio_runtime_store::StoreError) -> Vec<Cause> {
    use studio_runtime_store::StoreError;
    let detail = match error {
        StoreError::InvalidSyncObject { scope, id } => Some(Cause::StoredRecord {
            object: scope.clone(),
            id: id.clone(),
            pointer: None,
        }),
        StoreError::InvalidSyncObjectField {
            scope, id, pointer, ..
        } => Some(Cause::StoredRecord {
            object: scope.clone(),
            id: id.clone(),
            pointer: Some(pointer.clone()),
        }),
        StoreError::InvalidStoredRecord { object, id } => Some(Cause::StoredRecord {
            object: (*object).into(),
            id: id.clone(),
            pointer: None,
        }),
        StoreError::InvalidStoredObject { table, id } => Some(Cause::StoredRecord {
            object: (*table).into(),
            id: id.clone(),
            pointer: None,
        }),
        StoreError::SchemaObjectMismatch { object, .. } => Some(Cause::SchemaObject {
            object: object.clone(),
        }),
        StoreError::Json(error) => Some(json_cause(error)),
        StoreError::StateDirectory(error) => Some(Cause::Io {
            os_code: error.raw_os_error(),
        }),
        StoreError::Sqlite(error) | StoreError::CommitOutcomeUnknown(error) => match error {
            rusqlite::Error::SqliteFailure(error, _) => Some(Cause::Sqlite {
                primary_code: error.extended_code & SQLITE_PRIMARY_CODE_MASK,
                extended_code: error.extended_code,
            }),
            _ => None,
        },
        StoreError::AlreadyOwned
        | StoreError::MissingDatabase(_)
        | StoreError::UnsafeStateDirectory
        | StoreError::SchemaMismatch(_)
        | StoreError::SchemaValidation(_)
        | StoreError::RecordContract(_)
        | StoreError::WriterPoisoned
        | StoreError::WriterActorInitialization(_)
        | StoreError::WriterActorControl(_)
        | StoreError::CommitActorOutcomeUnknown(_)
        | StoreError::ReadTaskFailed => None,
    };
    let mut causes: Vec<_> = detail.into_iter().collect();
    causes.push(Cause::Runtime {
        class: store_error_kind(error),
    });
    causes
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_store::StoreError;

    #[test]
    fn typed_store_causes_keep_locations_and_codes_without_private_messages() {
        let field = StoreError::InvalidSyncObjectField {
            scope: "runtime_work".into(),
            id: "task-7".into(),
            pointer: "/dependencies/0".into(),
            reason: "password=source-canary".into(),
        };
        let causes = store_causes(&field);
        assert!(
            matches!(&causes[0], Cause::StoredRecord {object,id,pointer:Some(pointer)} if object=="runtime_work" && id=="task-7" && pointer=="/dependencies/0")
        );
        assert!(
            !serde_json::to_string(&causes)
                .expect("safe facts")
                .contains("source-canary")
        );
        let sql = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            Some("private SQL source-canary".into()),
        ));
        assert!(
            matches!(store_causes(&sql)[0], Cause::Sqlite {primary_code,extended_code} if primary_code==rusqlite::ffi::SQLITE_CONSTRAINT && extended_code==rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
        );
        assert!(
            !serde_json::to_string(&store_causes(&sql))
                .expect("safe facts")
                .contains("source-canary")
        );
        let json = serde_json::from_str::<serde_json::Value>("{\n  source-canary")
            .expect_err("syntax failure");
        assert!(matches!(
            json_cause(&json),
            Cause::Json {
                line: 2,
                category: JsonCategory::Syntax,
                ..
            }
        ));
    }
}
