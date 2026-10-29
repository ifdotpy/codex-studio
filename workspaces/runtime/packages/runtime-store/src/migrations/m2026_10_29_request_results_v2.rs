//! Add reference receipts while retaining existing exact bodies through their TTL.
use rusqlite::{OptionalExtension, Transaction, params};
use studio_runtime_schema::annotation_command::AnnotationData;
use studio_runtime_schema::persistence::{RequestReferenceKind, RequestResultStatus, enum_check};

pub(super) fn annotation_revision(tx: &Transaction<'_>) -> crate::StoreResult<()> {
    let present: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_annotations') WHERE name='rev')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        tx.execute_batch(
            "ALTER TABLE runtime_annotations ADD COLUMN rev INTEGER NOT NULL DEFAULT 1;",
        )?;
    }
    tx.execute_batch("CREATE TRIGGER IF NOT EXISTS runtime_annotation_revision AFTER UPDATE OF record ON runtime_annotations WHEN OLD.record IS NOT NEW.record BEGIN UPDATE runtime_annotations SET rev=OLD.rev+1 WHERE id=NEW.id; END;")?;
    Ok(())
}

pub(super) fn convert_annotations(tx: &Transaction<'_>) -> crate::StoreResult<()> {
    #[derive(serde::Deserialize)]
    struct OldResult {
        reply: AnnotationData,
    }
    let mut query = tx.prepare(
        "SELECT id,result FROM request_results WHERE id>=?1 AND id<?2 AND result IS NOT NULL",
    )?;
    let rows = query
        .query_map(["[\"annotation\",", "[\"annotation\"-"], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(query);
    for (id, result) in rows {
        let Ok((namespace, authority, root, operation)) =
            serde_json::from_str::<(String, Vec<String>, String, Vec<String>)>(&id)
        else {
            continue;
        };
        let Ok(old) = serde_json::from_str::<OldResult>(&result) else {
            continue;
        };
        if namespace != "annotation"
            || !operation.is_empty()
            || root != old.reply.id
            || authority.as_slice() != [old.reply.agent.as_str()]
        {
            continue;
        }
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_annotations WHERE id=?1",
                [&root],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = raw.and_then(|raw| serde_json::from_str::<AnnotationData>(&raw).ok())
        else {
            continue;
        };
        if old.reply != current {
            continue;
        }
        tx.execute(
            "UPDATE request_results SET ref_kind=?1,ref_id=?2,ref_rev=1,result=NULL WHERE id=?3",
            params![RequestReferenceKind::Annotation, root, id],
        )?;
    }
    Ok(())
}

pub(super) fn apply(tx: &Transaction<'_>) -> crate::StoreResult<()> {
    let migrated: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('request_results') WHERE name='status')",
        [],
        |row| row.get(0),
    )?;
    if migrated {
        return Ok(());
    }
    tx.execute_batch(&format!(
        "DROP INDEX IF EXISTS request_results_created;
         DROP INDEX IF EXISTS request_results_tool_actor_created;
         ALTER TABLE request_results RENAME TO request_results_v1;
         CREATE TABLE request_results (
           id TEXT PRIMARY KEY,
           signature TEXT NOT NULL,
           status TEXT NOT NULL DEFAULT '{applied}' {status},
           ref_kind TEXT {kind},
           ref_id TEXT,
           ref_rev INTEGER,
           result TEXT,
           created INTEGER NOT NULL,
           CHECK ((result IS NOT NULL AND ref_kind IS NULL AND ref_id IS NULL AND ref_rev IS NULL)
               OR (result IS NULL AND ref_kind IS NOT NULL AND ref_id IS NOT NULL AND ref_rev IS NOT NULL)
               OR (result IS NULL AND status = '{rejected}' AND ref_kind IS NULL AND ref_id IS NULL AND ref_rev IS NULL))
         );
         INSERT INTO request_results(id,signature,result,created)
           SELECT id,signature,result,created FROM request_results_v1;
         DROP TABLE request_results_v1;
         CREATE INDEX request_results_created ON request_results(created,id);
         CREATE INDEX request_results_tool_actor_created
           ON request_results(json_extract(result,'$.actor'),created DESC,id)
           WHERE json_extract(result,'$.actor') IS NOT NULL;",
        applied = RequestResultStatus::Applied.as_str(),
        rejected = RequestResultStatus::Rejected.as_str(),
        status = enum_check::<RequestResultStatus>("status"),
        kind = enum_check::<RequestReferenceKind>("ref_kind"),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn matching_legacy_annotation_becomes_a_reference_and_revision_advances() {
        use crate::request_results::ResultEnvelope;
        use studio_runtime_schema::annotation_command::AnnotationData;
        use studio_runtime_schema::persistence::RequestIdentity;
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE request_results(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL,created INTEGER NOT NULL); CREATE TABLE runtime_annotations(id TEXT PRIMARY KEY,record TEXT NOT NULL);").expect("old schema");
        let id = RequestIdentity::issue_root();
        let key = RequestIdentity::new("annotation", &["agent"], &id, &[]).expect("identity");
        let record = AnnotationData {
            id: id.clone(),
            agent: "agent".into(),
            root_id: "agent".into(),
            path: "file".into(),
            line: 1,
            text: "first".into(),
            created: 1.0,
            turn_id: None,
        };
        let body = serde_json::to_string(&ResultEnvelope {
            actor: Some("agent".into()),
            reply: &record,
            references: vec![],
        })
        .expect("body");
        db.execute(
            "INSERT INTO request_results VALUES(?1,'sig',?2,1)",
            rusqlite::params![key.id(), body],
        )
        .expect("legacy receipt");
        db.execute(
            "INSERT INTO runtime_annotations VALUES(?1,?2)",
            rusqlite::params![id, serde_json::to_string(&record).unwrap()],
        )
        .expect("annotation");
        let tx = db.transaction().expect("transaction");
        super::apply(&tx).expect("receipt migration");
        super::annotation_revision(&tx).expect("revision migration");
        super::convert_annotations(&tx).expect("convert legacy receipt");
        let saved: (String, String, i64, Option<String>) = tx
            .query_row(
                "SELECT ref_kind,ref_id,ref_rev,result FROM request_results WHERE id=?1",
                [key.id()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("reference");
        assert_eq!(saved, ("annotation".into(), id.clone(), 1, None));
        tx.execute(
            "UPDATE runtime_annotations SET record=json_set(record,'$.text','later') WHERE id=?1",
            [&id],
        )
        .expect("update");
        super::annotation_revision(&tx).expect("reopen");
        let revision: i64 = tx
            .query_row(
                "SELECT rev FROM runtime_annotations WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, 2);
    }

    #[test]
    fn reference_replay_preserves_revision_and_reads_current_row() {
        use crate::request_results::{AdmittedRequest, RequestReference, RequestResults};
        use studio_runtime_schema::persistence::RequestReferenceKind;
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE request_results(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL,created INTEGER NOT NULL); CREATE TABLE owner(id TEXT PRIMARY KEY, revision INTEGER NOT NULL, content TEXT NOT NULL); INSERT INTO owner VALUES('a',1,'first');").expect("old shape");
        let tx = db.transaction().expect("transaction");
        super::apply(&tx).expect("migration");
        let admitted = AdmittedRequest::from_migration("request", "signature", 42);
        let reference = RequestReference {
            kind: RequestReferenceKind::Annotation,
            id: "a".into(),
            revision: 1,
        };
        RequestResults::settle_reference(&tx, &admitted, &reference).expect("settle");
        tx.execute(
            "UPDATE owner SET revision=2,content='later' WHERE id='a'",
            [],
        )
        .expect("later write");
        let replay = RequestResults::replay_reference(&tx, "request", "signature", 43)
            .expect("replay")
            .expect("receipt");
        assert_eq!(replay, reference);
        let content: String = tx
            .query_row(
                "SELECT content FROM owner WHERE id=?1",
                [&replay.id],
                |row| row.get(0),
            )
            .expect("current row");
        assert_eq!(content, "later");
        assert!(RequestResults::replay_reference(&tx, "request", "changed", 43).is_err());
        let body: Option<String> = tx
            .query_row(
                "SELECT result FROM request_results WHERE id='request'",
                [],
                |row| row.get(0),
            )
            .expect("body");
        assert!(body.is_none());
    }

    #[test]
    fn old_shape_retains_exact_body_and_checks_reference_fields() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE request_results(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL,created INTEGER NOT NULL); INSERT INTO request_results(id,signature,result,created) VALUES('old','sig','{\"reply\":{\"outcome\":\"unknown\"}}',42);").expect("old shape");
        let tx = db.transaction().expect("transaction");
        super::apply(&tx).expect("migration");
        let row: (String, String, String, i64) = tx
            .query_row(
                "SELECT signature,status,result,created FROM request_results WHERE id='old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("preserved row");
        assert_eq!(
            row,
            (
                "sig".into(),
                "applied".into(),
                "{\"reply\":{\"outcome\":\"unknown\"}}".into(),
                42
            )
        );
        assert!(tx.execute("INSERT INTO request_results(id,signature,status,ref_kind,ref_id,ref_rev,created) VALUES('ref','sig','applied','annotation','a',1,42)", []).is_ok());
        assert!(
            tx.execute(
                "UPDATE request_results SET status='unknown' WHERE id='ref'",
                []
            )
            .is_err()
        );
        assert!(
            tx.execute(
                "UPDATE request_results SET ref_kind='bogus' WHERE id='ref'",
                []
            )
            .is_err()
        );
        assert!(
            tx.execute("UPDATE request_results SET ref_rev=NULL WHERE id='ref'", [])
                .is_err()
        );
        super::apply(&tx).expect("reopen leaves references intact");
        let revision: i64 = tx
            .query_row(
                "SELECT ref_rev FROM request_results WHERE id='ref'",
                [],
                |row| row.get(0),
            )
            .expect("reference survives");
        assert_eq!(revision, 1);
        tx.commit().expect("commit");
    }
}
