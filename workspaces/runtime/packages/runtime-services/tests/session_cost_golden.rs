//! Golden session-cost answers for a mixed Codex/Claude tree.
//!
//! `tests/costs/session-cost-golden.json` was produced by the row-scanning
//! implementation that the incremental analytics cost aggregate replaced.
//! Typed-contract adjustments are hand-audited below, not regenerated. The tree
//! has provisional and answered rows, model-less rows, a deleted agent, an agent without a record,
//! an account move, Claude log responses that replace analytics rows, several
//! models, a context tier, an unknown model and a row that changes turn and root.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use studio_runtime_services::costs::session_cost_with_account_profiles;
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository, CaptureTable};
use studio_runtime_store::{RuntimeStore, StoreError};

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/costs/session-cost-golden.json")
}

fn agent(id: &str, root: &str, provider: &str, account: &str, extra: Value) -> Value {
    let mut record = json!({"id":id,"rootId":root,"provider":provider,"accountKey":account,
        "threadId":format!("thread-{id}"),"accountHistory":[]});
    if let (Some(record), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        record.extend(extra.clone());
    }
    record
}

/// One usage row; `tokens` is `[input, cached, write, output]`.
#[allow(clippy::too_many_arguments)]
fn usage(
    id: &str,
    agent: &str,
    root: &str,
    turn: &str,
    at: f64,
    model: Option<&str>,
    account: &str,
    response: Option<&str>,
    tokens: [u64; 4],
    extra: Value,
) -> Value {
    let counts = json!({"inputTokens":tokens[0],"cachedInputTokens":tokens[1],
        "cacheWriteInputTokens":tokens[2],"outputTokens":tokens[3],
        "totalTokens":tokens[0] + tokens[3]});
    let mut record = json!({"id":id,"agentId":agent,"rootId":root,"threadId":format!("thread-{agent}"),
        "turnId":turn,"at":at,"accountKey":account,"model":model,"responseId":response,
        "last":counts,"total":counts});
    if let (Some(record), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        record.extend(extra.clone());
    }
    record
}

fn claude_line(id: &str, tokens: [u64; 4]) -> String {
    json!({"type":"assistant","timestamp":"2026-10-01T12:00:00Z","message":{
        "id":id,"model":"claude-sonnet-4-5","usage":{"input_tokens":tokens[0],
        "cache_read_input_tokens":tokens[1],"cache_creation_input_tokens":tokens[2],
        "output_tokens":tokens[3]}}})
    .to_string()
}

async fn fixture(root_dir: &Path) -> (RuntimeStore, BTreeMap<String, Value>) {
    let store = RuntimeStore::open(Some(root_dir))
        .await
        .expect("runtime store opens");
    let profile = root_dir.join("claude-profile");
    let project = profile.join("projects/project");
    fs::create_dir_all(&project).expect("Claude project");
    fs::create_dir_all(root_dir.join("account-servers/claude-acct/sessions"))
        .expect("Claude session state directory");
    fs::write(
        root_dir.join("account-servers/claude-acct/sessions/thread-claude-worker.json"),
        json!({"nativeId":"native-claude-worker"}).to_string(),
    )
    .expect("Claude session state");
    fs::write(
        project.join("thread-claude-worker.jsonl"),
        format!(
            "{}\n{}\n",
            claude_line("resp-c1", [1200, 300, 40, 90]),
            claude_line("resp-c-log-only", [700, 0, 0, 30])
        ),
    )
    .expect("Claude log");
    fs::write(
        project.join("native-claude-worker.jsonl"),
        format!("{}\n", claude_line("resp-c-native", [50, 10, 0, 5])),
    )
    .expect("Claude native log");
    fs::create_dir_all(root_dir.join("model-pricing")).expect("pricing directory");
    fs::write(
        root_dir.join("model-pricing/models-dev-v1.json"),
        json!({"version":1,"catalog":{"providers":{
            "openai":{"models":{
                "gpt-5":{"cost":{"input":1.25,"output":10,"cache_read":0.125}},
                "gpt-5-mini":{"cost":{"input":0.25,"output":2,"cache_read":0.025}},
                "gpt-long":{"cost":{"input":1,"output":2,"cache_read":0.5,
                    "tiers":[{"tier":{"type":"context","size":1000},"input":3,"output":6,"cache_read":1.5}]}}
            }},
            "anthropic":{"models":{
                "claude-sonnet-4-5":{"cost":{"input":3,"output":15,"cache_read":0.3,"cache_write":3.75}}
            }}
        }}})
        .to_string(),
    )
    .expect("pricing catalog");

    let agents = vec![
        agent("lead", "lead", "codex", "default", json!({})),
        agent("codex-worker", "lead", "codex", "default", json!({})),
        agent(
            "claude-worker",
            "lead",
            "claude",
            "claude-acct",
            json!({"accountHistory":[{"accountKey":"default","provider":"codex","threadId":"thread-old"}]}),
        ),
        agent(
            "deleted-worker",
            "lead",
            "codex",
            "second",
            json!({"deletedAt":1_700_000_100.0}),
        ),
        agent("other-root", "other", "codex", "default", json!({})),
    ];
    let mut rows = vec![
        // Lead: an answered turn with a provisional row (suppressed) and a
        // provisional-only turn (counted).
        usage(
            "u1",
            "lead",
            "lead",
            "t1",
            1.0,
            Some("gpt-5"),
            "default",
            None,
            [100, 0, 0, 10],
            json!({}),
        ),
        usage(
            "u2",
            "lead",
            "lead",
            "t1",
            2.0,
            Some("gpt-5"),
            "default",
            Some("r1"),
            [120, 20, 0, 12],
            json!({}),
        ),
        usage(
            "u3",
            "lead",
            "lead",
            "t2",
            3.0,
            Some("gpt-5"),
            "default",
            None,
            [80, 0, 0, 8],
            json!({}),
        ),
        // Same token tuple twice: one group of two.
        usage(
            "u4",
            "lead",
            "lead",
            "t3",
            4.0,
            Some("gpt-5"),
            "default",
            Some("r4"),
            [120, 20, 0, 12],
            json!({}),
        ),
        // Delta wins over last when it reports input and output. Its unknown
        // cache-write category keeps u5 unpriced under the typed contract;
        // the old scanner invented zero and charged (40*1.25 + 4*10)/1e6 = $0.000090.
        usage(
            "u5",
            "lead",
            "lead",
            "t3",
            5.0,
            Some("gpt-5"),
            "default",
            Some("r5"),
            [9999, 0, 0, 9999],
            json!({"delta":{"inputTokens":40,"cachedInputTokens":0,"cacheWriteInputTokens":null,"outputTokens":4}}),
        ),
        // Codex worker: model-less rows inherit from the turn, else the thread.
        usage(
            "w1",
            "codex-worker",
            "lead",
            "t1",
            1.0,
            Some("gpt-5-mini"),
            "default",
            Some("wr1"),
            [300, 100, 0, 30],
            json!({}),
        ),
        usage(
            "w2",
            "codex-worker",
            "lead",
            "t1",
            2.0,
            None,
            "default",
            Some("wr2"),
            [310, 100, 0, 31],
            json!({}),
        ),
        usage(
            "w3",
            "codex-worker",
            "lead",
            "t2",
            3.0,
            Some("gpt-long"),
            "default",
            Some("wr3"),
            [2000, 500, 0, 50],
            json!({}),
        ),
        usage(
            "w4",
            "codex-worker",
            "lead",
            "t9",
            9.0,
            None,
            "default",
            Some("wr4"),
            [10, 0, 0, 1],
            json!({}),
        ),
        usage(
            "w5",
            "codex-worker",
            "lead",
            "t2",
            3.5,
            Some("gpt-long"),
            "default",
            Some("wr5"),
            [500, 0, 0, 5],
            json!({"inputTokensAreUncached":true}),
        ),
        usage(
            "w6",
            "codex-worker",
            "lead",
            "t4",
            4.0,
            Some("mystery-model"),
            "default",
            Some("wr6"),
            [10, 0, 0, 1],
            json!({}),
        ),
        // Claude worker: resp-c1 is replaced by its log row; resp-c2 stays.
        // c1p has no response identity and is cumulative, never a priced response.
        // The old scanner revived c1p after replacing c1 with its journal receipt,
        // adding (10*3 + 1*15)/1e6 = $0.000045. Typed accounting excludes it.
        usage(
            "c1",
            "claude-worker",
            "lead",
            "ct1",
            1.0,
            Some("claude-sonnet-4-5"),
            "claude-acct",
            Some("resp-c1"),
            [1000, 300, 40, 80],
            json!({"inputTokensAreUncached":true}),
        ),
        usage(
            "c1p",
            "claude-worker",
            "lead",
            "ct1",
            0.5,
            Some("claude-sonnet-4-5"),
            "claude-acct",
            None,
            [10, 0, 0, 1],
            json!({}),
        ),
        usage(
            "c2",
            "claude-worker",
            "lead",
            "ct2",
            2.0,
            Some("claude-sonnet-4-5"),
            "claude-acct",
            Some("resp-c2"),
            [400, 100, 0, 20],
            json!({"inputTokensAreUncached":true}),
        ),
        // Deleted worker and an agent without a record still count.
        usage(
            "d1",
            "deleted-worker",
            "lead",
            "dt1",
            1.0,
            Some("gpt-5"),
            "second",
            Some("dr1"),
            [64, 0, 0, 6],
            json!({}),
        ),
        usage(
            "g1",
            "ghost-worker",
            "lead",
            "gt1",
            1.0,
            Some("gpt-5-mini"),
            "default",
            Some("gr1"),
            [32, 0, 0, 3],
            json!({}),
        ),
        usage(
            "g2",
            "ghost-worker",
            "lead",
            "gt1",
            1.5,
            Some("<synthetic>"),
            "default",
            Some("gr2"),
            [1, 0, 0, 1],
            json!({}),
        ),
        // Another root is not counted.
        usage(
            "o1",
            "other-root",
            "other",
            "ot1",
            1.0,
            Some("gpt-5"),
            "default",
            Some("or1"),
            [1_000_000, 0, 0, 1],
            json!({}),
        ),
        // Moves out of the lead tree, then back into turn t2 of the lead.
        usage(
            "m1",
            "lead",
            "other",
            "t1",
            6.0,
            Some("gpt-5"),
            "default",
            Some("rm"),
            [7, 0, 0, 7],
            json!({}),
        ),
    ];
    rows.push(usage(
        "m1",
        "lead",
        "lead",
        "t2",
        6.0,
        Some("gpt-5"),
        "default",
        Some("rm"),
        [7, 0, 0, 7],
        json!({}),
    ));
    // Answer the provisional-only turn t2 of the lead: u3 is now suppressed.
    rows.push(usage(
        "u6",
        "lead",
        "lead",
        "t2",
        7.0,
        Some("gpt-5"),
        "default",
        Some("r6"),
        [90, 0, 0, 9],
        json!({}),
    ));
    store
        .analytics_transaction::<_, StoreError>(move |tx, _| {
            for record in &agents {
                AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, record)?;
            }
            Ok(())
        })
        .await
        .expect("agents");
    // One transaction per usage write, as the projector applies them over time.
    for record in rows {
        store
            .analytics_transaction::<_, StoreError>(move |tx, _| {
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)
            })
            .await
            .expect("usage row");
    }
    let accounts = BTreeMap::from([
        ("default".to_owned(), json!({"provider":"openai"})),
        ("second".to_owned(), json!({})),
        (
            "claude-acct".to_owned(),
            json!({"provider":"claude","claudeOptions":{"configDir":profile}}),
        ),
    ]);
    (store, accounts)
}

#[tokio::test]
async fn session_cost_matches_the_row_scanning_golden_answers() {
    let temp = tempfile::tempdir().expect("temporary state");
    let (store, accounts) = fixture(temp.path()).await;
    let mut answers = BTreeMap::new();
    for agent in ["lead", "codex-worker", "claude-worker", "other-root"] {
        let answer =
            session_cost_with_account_profiles(&store, agent, &accounts).expect("session cost");
        answers.insert(agent, answer);
    }
    let answers = json!(answers);
    if std::env::var_os("STUDIO_WRITE_COST_GOLDEN").is_some() {
        fs::write(
            golden_path(),
            serde_json::to_string_pretty(&answers).expect("golden JSON") + "\n",
        )
        .expect("write golden answers");
    }
    let golden: Value =
        serde_json::from_slice(&fs::read(golden_path()).expect("golden answers")).expect("JSON");
    assert_eq!(answers, golden);
}
