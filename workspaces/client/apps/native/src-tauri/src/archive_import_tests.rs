use super::*;
use studio_client_core::backend::Backend;

fn tagged(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            json!(["array", values.into_iter().map(tagged).collect::<Vec<_>>()])
        }
        Value::Object(fields) => json!([
            "object",
            fields
                .into_iter()
                .map(|(key, value)| json!([key, tagged(value)]))
                .collect::<Vec<_>>()
        ]),
        value => json!(["value", value]),
    }
}
fn saved(name: &str, records: Vec<(Value, Value)>) -> Value {
    json!({"name":name,"records":records.into_iter().map(|(key,value)|json!([tagged(key),value])).collect::<Vec<_>>()})
}
fn archive() -> Value {
    let workspace = "a".repeat(32);
    let upload = json!({"id":"upload-old","workspace":workspace,"stateDir":"saved-state","agent":"chat","name":"old.bin","mime":"application/octet-stream","size":3,"created":1});
    let recording = json!({"id":"recording-old","chatId":"chat","created":2,"sampleRate":16000,"samples":2,"state":"ready","deletedAt":4,"transcriptionAttempt":"attempt-old","transcriptionState":"transcribing"});
    let outbox = json!({"body":{"id":"message-old","room":"chat","text":"saved"},"status":"uncertain","attempted":true,"created":1,"displayPending":true,"attachments":[]});
    json!({"version":1,"id":"archive-old","owners":{"root":"old-origin"},"snapshots":[{
        "version":1,"origin":"old-origin","preferences":{
            "theme":"dark",
            "studio-native-preferences-journal-v1":json!([{"id":"preference-old","method":"preferences.raw_set","params":{"key":"font","raw":"large"}}]).to_string()
        },
        "databases":[
            {"name":"codex-studio-uploads","stores":[saved("uploads",vec![(json!("upload-old"),tagged(upload))]),saved("contents",vec![(json!("upload-old"),json!(["object",[["id",["value","upload-old"]],["bytes",["bytes","AP+A"]]]]))])]},
            {"name":"codex-studio-dictation","stores":[saved("recordings",vec![(json!("recording-old"),tagged(recording))]),saved("chunks",vec![(json!(["recording-old",0]),json!(["object",[["id",["value","recording-old"]],["index",["value",0]],["pcm",["view","Int16Array",["bytes","AQACAA=="]]]]]))])]},
            {"name":format!("rxdb-dexie-studio{workspace}--0--outbox"),"stores":[saved("docs",vec![(json!("message-old"),tagged(json!({"id":"message-old","payload":outbox.to_string(),"seq":1,"_deleted":false})))])]}
        ]
    }]})
}
async fn context(path: &std::path::Path) -> Context {
    let client = std::sync::Arc::new(
        Client::open_with_backend(
            path.join("owned"),
            Backend::new("http://127.0.0.1:9").unwrap(),
            |_| {},
        )
        .unwrap(),
    );
    client
        .request(Request {
            id: "fixture-identity".into(),
            method: "identity.set".into(),
            params: json!({"workspaceId":"a".repeat(32)}),
        })
        .await
        .unwrap()
        .result
        .unwrap();
    Context {
        server_id: "local".into(),
        workspace_id: "a".repeat(32),
        client,
        backend: None,
        source_identity: "b".repeat(64),
        migration_complete: Default::default(),
        archive_hold: true,
    }
}
async fn value(context: &Context, method: &str, params: Value) -> Value {
    context
        .client
        .request(Request {
            id: format!("inspect:{method}"),
            method: method.into(),
            params,
        })
        .await
        .unwrap()
        .result
        .unwrap()
}

#[tokio::test]
async fn native_host_archive_prepare_preserves_bytes_ids_uncertainty_and_keeps_the_hold() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let source = archive();
    let path = directory.path().join("archive.json");
    let selected = crate::profile::select_sealed(&source, &path, "root").unwrap();
    let prepared = prepare(&context, &selected).await.unwrap();
    assert_eq!(prepared["prepared"], true);
    assert_eq!(prepared["complete"], false);
    assert_eq!(prepare(&context, &selected).await.unwrap(), prepared);
    assert_eq!(
        value(&context, "preferences.raw_all", json!({})).await["font"],
        "large"
    );
    assert_eq!(
        value(&context, "uploads.content", json!({"id":"upload-old"})).await,
        "AP+A"
    );
    let recording = value(
        &context,
        "dictation.get",
        json!({"id":"recording-old","chatId":"chat"}),
    )
    .await;
    assert_eq!(recording["transcriptionAttempt"], "attempt-old");
    assert_eq!(recording["transcriptionState"], "transcribing");
    assert_eq!(recording["deletedAt"], 4);
    let outbox = value(&context, "outbox.list", json!({"workspace":"a".repeat(32)})).await;
    assert_eq!(outbox[0]["id"], "message-old");
    assert_eq!(outbox[0]["attempted"], true);
    assert_eq!(outbox[0]["status"], "uncertain");
    let saved_pcm = prepared["bytes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|receipt| receipt["receipt"]["entries"].as_array().unwrap())
        .find(|entry| entry["id"] == "recording-old")
        .unwrap();
    assert_eq!(saved_pcm["sha256"], hash(&[1, 0, 2, 0]));
    assert_eq!(
        value(
            &context,
            "client_storage.cutover_status",
            json!({"sourceIdentity":context.source_identity,"serverId":"local"})
        )
        .await["complete"],
        false
    );
    assert!(
        context
            .ensure_ready(&Request {
                id: "effect".into(),
                method: "outbox.deliver".into(),
                params: json!({})
            })
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_host_archive_prepare_reconciles_a_committed_partial_import_and_lost_reply() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let source = archive();
    let path = directory.path().join("archive.json");
    let selected = crate::profile::select_sealed(&source, &path, "root").unwrap();
    let planned = plan(&context, &selected).unwrap();
    let artifact = &planned.artifacts[0];
    let id = &planned.metadata["expectedByteImports"][0];
    call(&context.client,"files.importBegin",json!({"manifest":{"version":1,"id":id,"source":context.source_identity,"workspace":context.workspace_id,"entries":[artifact.entry]}})).await.unwrap();
    call(
        &context.client,
        "files.importChunk",
        json!({"manifestId":id,"entry":0,"offset":0,"base64":STANDARD.encode(&artifact.bytes)}),
    )
    .await
    .unwrap();
    let committed = call(
        &context.client,
        "files.importCommit",
        json!({"manifestId":id}),
    )
    .await
    .unwrap();
    let first = prepare(&context, &selected).await.unwrap();
    let repeated = prepare(&context, &selected).await.unwrap();
    assert_eq!(first, repeated);
    assert_eq!(first["bytes"][0]["receipt"], committed);
    assert_eq!(first["complete"], false);
    let mut changed = source.clone();
    changed["snapshots"][0]["preferences"]["theme"] = json!("changed");
    let changed = crate::profile::select_sealed(&changed, &path, "root").unwrap();
    assert!(prepare(&context, &changed).await.is_err());
    assert_eq!(
        value(&context, "preferences.raw_all", json!({})).await["theme"],
        "dark"
    );
    // A later owner cancellation is not overwritten by an old archive retry.
    call(
        &context.client,
        "uploads.cancel",
        json!({"id":"upload-old"}),
    )
    .await
    .unwrap();
    assert!(prepare(&context, &selected).await.is_err());
    assert_eq!(
        value(&context, "uploads.get", json!({"id":"upload-old"})).await,
        Value::Null
    );
}

#[tokio::test]
async fn native_host_archive_prepare_reads_verified_version_two_parts() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let mut source = archive();
    source["version"] = json!(2);
    source["snapshots"][0]["version"] = json!(2);
    source["parts"] = json!("parts");
    std::fs::create_dir(directory.path().join("parts")).unwrap();
    let stores = source["snapshots"][0]["databases"].as_array_mut().unwrap();
    for db in stores {
        for store in db["stores"].as_array_mut().unwrap() {
            let object = store.as_object_mut().unwrap();
            let records = object.remove("records").unwrap();
            let bytes = serde_json::to_vec(&records).unwrap();
            let digest = hash(&bytes);
            std::fs::write(
                directory
                    .path()
                    .join("parts")
                    .join(format!("{digest}.json")),
                &bytes,
            )
            .unwrap();
            object.insert("chunks".into(),json!([{"sha256":digest,"bytes":bytes.len(),"records":records.as_array().unwrap().len()}]));
        }
    }
    let path = directory
        .path()
        .canonicalize()
        .unwrap()
        .join("archive.json");
    let selected = crate::profile::select_sealed(&source, &path, "root").unwrap();
    assert_eq!(
        prepare(&context, &selected).await.unwrap()["prepared"],
        true
    );
    let digest = source["snapshots"][0]["databases"][0]["stores"][0]["chunks"][0]["sha256"]
        .as_str()
        .unwrap();
    std::fs::write(
        directory
            .path()
            .join("parts")
            .join(format!("{digest}.json")),
        b"[]",
    )
    .unwrap();
    assert!(prepare(&context, &selected).await.is_err());
}

#[tokio::test]
async fn native_host_archive_unknown_source_is_rejected_before_any_import() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let mut source = archive();
    source["snapshots"][0]["databases"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"unknown-user-data","stores":[]}));
    let path = directory.path().join("archive.json");
    let selected = crate::profile::select_sealed(&source, &path, "root").unwrap();
    assert!(
        prepare(&context, &selected)
            .await
            .unwrap_err()
            .contains("Unsupported")
    );
    assert_eq!(
        value(&context, "preferences.raw_all", json!({})).await,
        json!({})
    );
    assert_eq!(
        value(&context, "uploads.get", json!({"id":"upload-old"})).await,
        Value::Null
    );
}

#[test]
fn native_host_archive_selector_rejects_renderer_sources_and_empty_inventories() {
    for extra in [
        json!({"metadata":[],"bytes":[]}),
        json!({"serializedSnapshot":"{}"}),
        json!({"archive":{"id":"real-label"}}),
    ] {
        let mut request = json!({"serverId":"local","token":"host-cutover-token"});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(serde_json::from_value::<crate::service_context::CutoverCheck>(request).is_err());
    }
}

async fn legacy_mutation_owner_reopen(path: &str, serialized: &str, explicit: Option<&str>) {
    for plain in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut context = context(directory.path()).await;
        context.server_id = "remote".into();
        let payload_digest = hash(format!("{path}\n{serialized}").as_bytes());
        let request_id = explicit.unwrap_or("original-generic-request");
        let saved = if plain {
            request_id.to_owned()
        } else {
            json!({"requestId":request_id,"uncertain":true}).to_string()
        };
        let prefix = crate::profile::migration_policy()["legacyMutationStoragePrefix"]
            .as_str()
            .unwrap();
        let mut preferences = serde_json::Map::new();
        preferences.insert(
            format!(":server:remote:{prefix}{payload_digest}"),
            json!(saved),
        );
        let source = json!({"version":1,"id":"legacy-mutation-source","owners":{"server:remote":"saved-origin"},"snapshots":[{"version":1,"origin":"saved-origin","preferences":preferences,"databases":[]}]});
        let source_path = directory.path().join("source.json");
        let selected =
            crate::profile::select_sealed(&source, &source_path, "server:remote").unwrap();
        let planned = plan(&context, &selected).unwrap();
        let import = Request {
            id: "original-import-call".into(),
            method: "client_storage.import".into(),
            params: json!({"manifestId":"legacy-mutation-import","serializedSource":planned.metadata.to_string()}),
        };
        let database = directory.path().join("reopened-owner.sqlite");
        {
            let mut owner = studio_client_core::store::Store::open(&database).unwrap();
            let response = owner.dispatch(Request {
                id: import.id.clone(),
                method: import.method.clone(),
                params: import.params.clone(),
            });
            assert!(response.error.is_none());
            // The caller loses this success response before its next process opens the owner.
            drop(response);
        }
        let mut owner = studio_client_core::store::Store::open(&database).unwrap();
        assert!(owner.dispatch(import).error.is_none());
        let mut params = json!({"path":path,"serializedPayload":serialized});
        if let Some(explicit) = explicit {
            params["explicitRequestId"] = json!(explicit);
        }
        let response = owner.dispatch(Request {
            id: "retry-call".into(),
            method: "mutation.begin".into(),
            params,
        });
        let entry = response
            .result
            .expect("The original legacy mutation must recover through the native journal owner");
        assert_eq!(
            entry["requestId"], request_id,
            "A reopened owner cannot mint another operation ID"
        );
        assert_eq!(entry["payloadDigest"], payload_digest);
        assert_eq!(
            entry["uncertain"], true,
            "Restart cannot turn an unresolved write into NotSent"
        );
        if explicit.is_some() {
            for (id, payload) in [
                ("different-operation", serialized.to_owned()),
                (request_id, format!(" {serialized}")),
            ] {
                let refused = owner.dispatch(Request {
                    id: format!("changed-{id}"),
                    method: "mutation.begin".into(),
                    params: json!({"path":path,"serializedPayload":payload,"explicitRequestId":id}),
                });
                assert!(
                    refused.error.is_some(),
                    "Changed identity or exact serialized payload must remain refused"
                );
            }
        }
    }
}

#[tokio::test]
async fn native_host_archive_legacy_generic_identity_survives_lost_reply_and_owner_reopen() {
    legacy_mutation_owner_reopen(
        "/api/ui-preferences",
        r#"{"scope":"user","fields":[]}"#,
        None,
    )
    .await;
}

#[tokio::test]
async fn native_host_archive_retains_reset_credit_identity_and_uncertainty_after_owner_reopen() {
    // This uses the real reset request field order, including its required original request_id.
    // It checks journal recovery only. No provider or credit redemption is called.
    legacy_mutation_owner_reopen("/api/limits/reset",r#"{"credit_id":"saved-credit","account_id":"saved-account","account_key":"saved-key","request_id":"reset-original"}"#,Some("reset-original")).await;
}

fn current_capture(preferences: Value) -> crate::legacy_capture::CapturedSource {
    crate::legacy_capture::CapturedSource {
        top_origin: "tauri://localhost".into(),
        origin: "tauri://localhost".into(),
        document_token: "private-document".into(),
        snapshot: json!({"version":1,"origin":"tauri://localhost","preferences":preferences,"databases":[]}),
        digest: "c".repeat(64),
    }
}

#[tokio::test]
async fn native_host_capture_known_empty_rxdb_auxiliary_stores_keep_original_outbox() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let mut source = archive();
    let stores = source["snapshots"][0]["databases"][2]["stores"]
        .as_array_mut()
        .unwrap();
    stores.push(saved("changes", vec![]));
    stores.push(saved("attachments", vec![]));
    let selected =
        crate::profile::select_sealed(&source, std::path::Path::new(""), "root").unwrap();
    let planned = plan(&context, &selected)
        .expect("Known empty auxiliary stores cannot hide the original user outbox");
    assert_eq!(planned.metadata["outbox"][0]["id"], "message-old");
    source["snapshots"][0]["databases"][2]["stores"][1] = saved(
        "changes",
        vec![(json!("unclassified"), tagged(json!({"pending":true})))],
    );
    let selected =
        crate::profile::select_sealed(&source, std::path::Path::new(""), "root").unwrap();
    assert!(
        plan(&context, &selected).is_err(),
        "A nonempty auxiliary source cannot be excluded without its own maintained reader"
    );
}

#[tokio::test]
async fn native_host_capture_keeps_file_metadata_in_addition_to_upload_document_and_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    for modified in [json!(123), json!(-10)] {
        let mut source = archive();
        source["snapshots"][0]["databases"][0]["stores"][1] = saved(
            "contents",
            vec![(
                json!("upload-old"),
                json!([
                    "object",
                    [
                        ["id", ["value", "upload-old"]],
                        [
                            "file",
                            [
                                "file",
                                "original-file.bin",
                                "application/original",
                                modified,
                                ["bytes", "AP+A"]
                            ]
                        ]
                    ]
                ]),
            )],
        );
        let selected =
            crate::profile::select_sealed(&source, std::path::Path::new(""), "root").unwrap();
        let planned = plan(&context, &selected)
            .expect("Original File metadata is distinct from the upload document");
        let artifact = planned
            .artifacts
            .iter()
            .find(|artifact| artifact.entry["id"] == "upload-old")
            .unwrap();
        let key = crate::profile::migration_policy()["fileMetadataField"]
            .as_str()
            .unwrap();
        assert_eq!(artifact.entry["document"]["name"], "old.bin");
        assert_eq!(
            artifact.entry["document"][key],
            json!({"kind":"file","name":"original-file.bin","mime":"application/original","lastModified":modified})
        );
        assert_eq!(artifact.bytes, vec![0, 255, 128]);
    }
}

#[tokio::test]
async fn native_host_capture_projection_cache_has_explicit_byte_inventory_and_unknown_tags_hold() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let mut source = archive();
    let cache_record = (
        json!("derived-row"),
        tagged(json!({"id":"derived-row","payload":"saved server projection"})),
    );
    source["snapshots"][0]["databases"].as_array_mut().unwrap().push(json!({"name":format!("rxdb-dexie-studio{}--0--projections",context.workspace_id),"stores":[saved("docs",vec![cache_record]),saved("changes",vec![]),saved("attachments",vec![])]}));
    let selected =
        crate::profile::select_sealed(&source, std::path::Path::new(""), "root").unwrap();
    let planned = plan(&context, &selected)
        .expect("A maintained projection-cache policy needs exact captured record evidence");
    assert_eq!(planned.caches.len(), 3);
    assert_eq!(planned.caches[0]["records"], 1);
    assert!(crate::profile::valid_sha256(
        planned.caches[0]["sha256"].as_str().unwrap()
    ));
    assert_eq!(
        planned.metadata["archiveManifest"]["excludedCaches"],
        json!(planned.caches)
    );
    assert_eq!(
        planned.artifacts.len(),
        2,
        "User upload and recording bytes retain their original owners"
    );
    source["snapshots"][0]["databases"][3]["stores"][0]["records"][0][1] =
        json!(["unknown-structured-value", "retained"]);
    let selected =
        crate::profile::select_sealed(&source, std::path::Path::new(""), "root").unwrap();
    assert!(plan(&context, &selected).is_err());
}

#[tokio::test]
async fn native_host_current_capture_keeps_original_native_preferences_and_physical_draft_ids() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let original = context
        .client
        .request(Request {
            id: "original-native-preference".into(),
            method: "preferences.raw_set".into(),
            params: json!({"key":"browser:\"root\":theme","raw":"current-native"}),
        })
        .await
        .unwrap();
    assert!(original.error.is_none());
    let native = call(
        &context.client,
        "client_storage.export_preferences",
        json!({"scope":"root"}),
    )
    .await
    .unwrap();
    let key = format!(
        "studio-native-draft-journal-v1:{}:original-draft-edit",
        context.workspace_id
    );
    let capture = current_capture(
        json!({"theme":"stale-browser","studio-native-preferences-ready-v1":"1",key:json!({"version":1,"workspace":context.workspace_id,"sequence":1,"edit":{"id":"original-draft-edit","changes":{"chat":"saved draft"}}}).to_string()}),
    );
    let planned = current_plan(&context, &capture, &native).unwrap();
    assert_eq!(
        planned.metadata["preferences"]["theme"], "current-native",
        "The original SQLite owner is authoritative after native preference adoption"
    );
    assert_eq!(
        planned.metadata["preferenceReceipts"][0]["id"],
        "original-native-preference"
    );
    assert_eq!(
        planned.metadata["pendingDrafts"][0]["id"],
        "original-draft-edit"
    );
    assert!(
        planned.metadata.get("archiveManifest").is_none(),
        "A current capture must not invent a sealed archive label"
    );
}

#[tokio::test]
async fn native_host_current_capture_refuses_a_different_original_native_owner() {
    let directory = tempfile::tempdir().unwrap();
    let context = context(directory.path()).await;
    let capture = current_capture(json!({}));
    assert!(current_plan(&context,&capture,&json!({"version":1,"legacyPreferenceScope":"server:other","preferences":{},"preferenceReceipts":[]})).is_err());
}
