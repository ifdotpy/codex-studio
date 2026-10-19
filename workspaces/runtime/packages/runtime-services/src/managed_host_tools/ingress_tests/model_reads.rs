//! `orchestration_read` and `orchestration_context` against Python's
//! `model_read` and `model_context` on the same Python-written rows and blobs.

use super::*;

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

async fn model_reads_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let source = PathBuf::from(template().value["modelReads"]["template"].as_str().unwrap());
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(source.join(name), directory.path().join(name)).unwrap();
    }
    copy_tree(&source.join("blobs"), &directory.path().join("blobs"));
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    (directory, store)
}

fn actor_context(id: &Value) -> CallbackContext {
    let actor = template().value["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|actor| actor["id"] == *id)
        .unwrap();
    CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().into(),
        turn: actor["turnId"].as_str().unwrap().into(),
    }
}

fn answered(decision: CallbackDecision) -> (bool, String) {
    let CallbackDecision::Answer(reply) = decision else {
        panic!("tool callback must answer: {decision:?}")
    };
    let ResponseBody::Result(result) = reply.body else {
        panic!("tool callback result expected")
    };
    (
        result["success"] == true,
        result["contentItems"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

#[tokio::test]
async fn model_read_matches_python_for_events_native_output_and_saved_payloads() {
    let (_root, store) = model_reads_fixture().await;
    let context = actor_context(&template().value["agents"][0]["id"]);
    let cases = template().value["modelReads"]["reads"].as_array().unwrap();
    assert!(cases.len() >= 17, "the Python read corpus is incomplete");
    for (index, case) in cases.iter().enumerate() {
        let frame = call(
            &context,
            &format!("model-read-{index}"),
            "orchestration_read",
            case["arguments"].clone(),
        );
        let (success, text) = answered(
            crate::provider_callbacks::dispatch(Arc::clone(&store), &context, &frame, 20.0)
                .await
                .unwrap(),
        );
        assert_eq!(
            success,
            case["success"] == true,
            "{}: {text}",
            case["arguments"]
        );
        if success {
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap(),
                case["value"],
                "{}",
                case["arguments"]
            );
        } else {
            assert_eq!(
                text,
                case["error"].as_str().unwrap(),
                "{}",
                case["arguments"]
            );
        }
    }
}

#[tokio::test]
async fn model_context_topics_match_python() {
    let (_root, store) = model_reads_fixture().await;
    let resources = Arc::new(
        crate::account_worker::preparation::PreparationResources::bundled(
            String::new(),
            crate::model_catalog::ModelCatalogService::new(),
        )
        .unwrap(),
    );
    let reads = &template().value["modelReads"];
    let cases = reads["contexts"].as_array().unwrap();
    assert_eq!(cases.len(), 10, "the Python context corpus is incomplete");
    for (index, case) in cases.iter().enumerate() {
        let context = actor_context(&case["actor"]);
        let frame = call(
            &context,
            &format!("model-context-{index}"),
            "orchestration_context",
            json!({"topic":case["topic"]}),
        );
        let mut config = managed_ports(None, None);
        config.preparation = Some(Arc::clone(&resources));
        let ports = shared_ports(
            crate::provider_callbacks::managed_tool_ports_with_runtime(
                &store, &context, &frame, config,
            )
            .unwrap(),
        );
        let (success, text) = answered(
            dispatch_with_managed_tools(Arc::clone(&store), &context, &frame, 20.0, ports)
                .await
                .unwrap(),
        );
        let label = format!("{} for {}", case["topic"], case["actor"]);
        assert_eq!(success, case["success"] == true, "{label}: {text}");
        if !success {
            assert_eq!(text, case["error"].as_str().unwrap(), "{label}");
            continue;
        }
        let mut actual: Value = serde_json::from_str(&text).unwrap();
        if actual["truncated"] == true {
            // A large reply is a preview; the saved result holds the value.
            let saved = studio_runtime_store::system_reads::get(
                &store.read().unwrap(),
                "runtime_tool_results",
                actual["outputRef"].as_str().unwrap(),
            )
            .unwrap()
            .unwrap();
            actual =
                serde_json::from_str(saved["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        }
        let expected = &case["value"];
        assert_eq!(actual["topic"], expected["topic"], "{label}");
        let record = studio_runtime_store::system_reads::get(
            &store.read().unwrap(),
            "runtime_agents",
            &context.agent,
        )
        .unwrap()
        .unwrap();
        let progress = || {
            crate::account_worker::preparation::progress_context(
                &resources,
                &record,
                store.state_dir(),
            )
            .unwrap()
        };
        match case["topic"].as_str().unwrap() {
            // Python and Rust digest these sorted rows to the same version.
            "complaints" | "profiles" => assert_eq!(actual, *expected, "{label}"),
            // Python digests definitions in file key order, Rust in sorted order.
            "tools" => assert_eq!(actual["content"], expected["content"], "{label}"),
            // The progress text names this store's state directory and the Rust
            // layout checker; the panel skill text is Python's exact text.
            "panel" => assert_eq!(
                actual["content"],
                json!(format!(
                    "{}\n\n{}",
                    progress(),
                    reads["panelGuidance"].as_str().unwrap()
                )),
                "{label}"
            ),
            "background" => {
                assert_eq!(actual["content"]["workflow"], json!(progress()), "{label}");
                for field in ["monitor", "polling"] {
                    assert_eq!(
                        actual["content"][field], expected["content"][field],
                        "{label}"
                    );
                }
            }
            topic => panic!("unexpected topic {topic}"),
        }
    }
}
