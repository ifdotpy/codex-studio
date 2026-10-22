use super::*;

fn input_text(prepared: &turn::PreparedSubmission) -> String {
    prepared
        .provider_inputs
        .iter()
        .filter_map(|input| match input {
            turn::ProviderInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Exercise the store projection and receipt manifest through the actual turn
/// compiler used by both providers, including parent and monitor-only workers.
#[tokio::test]
async fn post_compaction_roster_uses_live_store_and_stops_after_delivery() {
    for provider in ["codex", "claude"] {
        let directory = tempfile::tempdir().expect("temporary state directory creates");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        let state_root = directory.path().to_path_buf();
        let resources = PreparationResources::bundled(
            directory.path().display().to_string(),
            crate::model_catalog::ModelCatalogService::new(),
        )
        .expect("bundled resources load");
        store.transaction_with_error(move |tx| -> Result<(), WorkerError> {
            for (id, root, parent, lead) in [
                ("lead","lead",None,true), ("parent","lead",Some("lead"),false),
                ("nested","lead",Some("parent"),false), ("monitor-owner","lead",Some("lead"),false),
                ("other","other",None,true),
            ] {
                let record = json!({"id":id,"rootId":root,"parentId":parent,"isLead":lead,"epoch":1,
                    "status":"running","name":id,"autoWake":true,"cwd":"/work","provider":provider,
                    "accountKey":"default","threadId":format!("{id}-thread"),"turnId":"turn","inFlight":true,"events":0,"compactions":0});
                AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(record).expect("agent fixture deserializes"))?;
            }
            for index in 0..30 {
                let id = format!("worker-{index:02}");
                AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(json!({
                    "id":id,"rootId":"lead","parentId":"lead","isLead":false,"epoch":1,"status":"running",
                    "name":format!("Worker {index}"),"autoWake":true,"cwd":"/work","provider":provider,
                    "accountKey":"default"})).expect("worker fixture deserializes"))?;
            }
            // Only the volatile status changes. A direct JSON read would lie.
            let mut finished = AgentRepository::get(tx, &"worker-29".into())?.expect("finished worker exists").value();
            finished["status"] = json!("completed");
            studio_runtime_store::agent_volatile::update(tx, "worker-29", &finished)?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
            for (id, owner) in [("watch-lead","lead"),("watch-worker","monitor-owner"),("watch-other","other")] {
                tx.execute("INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)", rusqlite::params![id,
                    json!({"id":id,"agent":owner,"status":"running","command":"check"}).to_string()])?;
            }
            for (id, owner, status) in [("assignment","worker-00","running"),("pending-review","nested","review"),("closed","worker-01","accepted")] {
                tx.execute("INSERT INTO runtime_work(id,record) VALUES(?1,?2)", rusqlite::params![id,
                    json!({"id":id,"rootId":"lead","owner":owner,"title":id,"status":status,"createdBy":"parent","results":[{"text":"evidence"}]}).to_string()])?;
            }
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('uncollected','lead','child_result','result','pending',1,1)", [])?;
            for (index, actor) in ["lead", "parent", "monitor-owner"].into_iter().enumerate() {
                let mut record = AgentRepository::get(tx, &actor.into())?.expect("actor exists").value();
                let intent: TurnIntent = serde_json::from_value(json!({"agent_id":actor,"epoch":1,
                    "attempt_id":"prepare","account_key":"default","provider":provider,
                    "thread_id":format!("{actor}-thread"),"cwd":"/work","model":null,
                    "events":[{"id":"wake","kind":"user","text":"continue","created":1.0,"epoch":1,"error":null,"metadata":{}}]})).expect("turn intent deserializes");
                let (before, events) = submission_snapshot(tx, &resources, &record, &intent, &state_root)?;
                assert!(before.team_state.is_none());
                assert!(!input_text(&turn::compile(&before, &events).expect("initial turn compiles")).contains("[Studio team state after compaction]"));
                let notification = json!({"method":"item/completed","params":{
                    "threadId":format!("{actor}-thread"),"turnId":"turn",
                    "item":{"id":format!("compact-{actor}"),"type":"contextCompaction"}}});
                let frame = studio_runtime_providers::LineDecoder::new().push(format!("{notification}\n").as_bytes()).expect("compaction notification decodes").remove(0);
                crate::notification_dispatch::dispatch_notification(
                    tx, actor, &crate::notification_dispatch::PublicationContext::default(),
                    &crate::notification_dispatch::NotificationStreamBuffer::default(),
                    &crate::notification_dispatch::ProviderDeliveryIdentity {provider_handle:"account:default".into(),generation:1,cursor:index as u64 + 1},
                    &frame, 2.0,
                )?;
                record = AgentRepository::get(tx, &actor.into())?.expect("actor remains after compaction").value();
                assert_eq!(record["compactions"], 1);
                let (snapshot, events) = submission_snapshot(tx, &resources, &record, &intent, &state_root)?;
                let restored = turn::compile(&snapshot, &events).expect("restored turn compiles");
                let text = input_text(&restored);
                assert_eq!(text.matches("[Studio team state after compaction]").count(), 1, "{actor}");
                assert!(!text.contains("watch-other"));
                if actor == "lead" {
                    assert!(text.contains("worker-00"));
                    assert!(text.contains("assignment"));
                    assert!(text.contains("pending-review"));
                    assert!(text.contains("event:uncollected"));
                    assert!(text.contains("resultsAwaitingReview"));
                    assert!(!text.contains("worker-29"));
                    assert!(!text.contains("\"closed\""));
                } else if actor == "parent" {
                    assert!(text.contains("nested"));
                    assert!(text.contains("pending-review"));
                    assert!(!text.contains("watch-lead"));
                } else {
                    assert!(text.contains("watch-worker"));
                    assert!(!text.contains("nested"));
                }
                tx.execute("INSERT INTO runtime_context_manifests(agent,thread_id,compactions,sequence,event_id,record) VALUES(?1,?2,1,?3,'wake',?4)", rusqlite::params![actor, format!("{actor}-thread"),i64::try_from(restored.manifest.sequence).expect("manifest sequence fits i64"),json!(restored.manifest).to_string()])?;
                let (snapshot, events) = submission_snapshot(tx, &resources, &record, &intent, &state_root)?;
                assert!(snapshot.team_state.is_none());
                assert!(!input_text(&turn::compile(&snapshot, &events).expect("turn after delivery compiles")).contains("[Studio team state after compaction]"));
            }
            Ok(())
        }).await.expect("compaction fixture transaction succeeds");
    }
}

/// The real preparation adapter publishes the immutable blob before metadata
/// can refer to it, and replays produce the same reference and bytes.
#[tokio::test]
async fn aggregate_spill_is_materialized_before_compilation_is_persisted() {
    let directory = tempfile::tempdir().expect("temporary state directory creates");
    let store = RuntimeStore::open(Some(directory.path()))
        .await
        .expect("runtime store opens");
    let snapshot: turn::PreparationSnapshot = serde_json::from_value(json!({
        "actor":{"id":"lead","root_id":"lead","epoch":["thread",1],"is_lead":true,
            "forked_from":null,"prepared_context":null,"delivered_mode":null},
        "root":{"concurrency":32,"mode_revision":0},"delivered_manifests":[],"plan":null,
        "complaints":[],"workers":[],"delivered_reminders":[],"assets":[],
        "guidance":{"role":"role".repeat(30_000),"progress":"progress.md"},"artifact_paths":{},
        "prepared_at":1.0,"operation_id":"prepare"
    }))
    .expect("preparation snapshot deserializes");
    let events: Vec<turn::Event> = serde_json::from_value(json!([{
        "id":"wake","kind":"user","text":"continue","created":1.0,
        "metadata":{"assets":[],"accepted_at":null,"extra":{}},
        "preserve_progress":false,"preserve_complaint":false
    }]))
    .expect("turn events deserialize");
    let (_, _, compiled) = compile_with_artifacts(&store, snapshot.clone(), events.clone())
        .await
        .expect("initial compilation materializes");
    let expected = compiled
        .studio_input
        .as_ref()
        .expect("compiled turn contains studio input")
        .as_bytes()
        .to_vec();
    let reference = compiled.metadata_patches["wake"]["studioInput"].clone();
    assert_eq!(
        crate::payload_blobs::load_blob(directory.path(), &reference)
            .expect("published payload loads"),
        expected
    );
    assert!(input_text(&compiled).len() - "continue".len() <= turn::STUDIO_INPUT_BYTES);
    let (_, _, replay) = compile_with_artifacts(&store, snapshot, events)
        .await
        .expect("replay compilation materializes");
    assert_eq!(input_text(&compiled), input_text(&replay));
    store
        .transaction_with_error(move |tx| -> Result<(), WorkerError> {
            persist_compilation(tx, &compiled)?;
            let raw: String = tx.query_row(
                "SELECT record FROM runtime_event_meta WHERE id='wake'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(
                serde_json::from_str::<Value>(&raw).expect("stored event metadata parses")["studioInput"],
                reference
            );
            Ok(())
        })
        .await
        .expect("compilation persists");
}
