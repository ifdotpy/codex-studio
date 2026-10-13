use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use studio_runtime_services::cost_usage::CostUsageSnapshot;
use studio_runtime_services::pricing::PricingCatalog;
use studio_runtime_services::token_rate::TokenRateSnapshot;

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn python() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("CODEX_AGENTS_PYTHON")
            .map(PathBuf::from)
            .expect("set CODEX_AGENTS_PYTHON to the prepared interpreter")
    })
    .clone()
}

#[test]
fn pricing_and_grouping_match_real_python_oracles() {
    let catalog = json!({"providers": {
        "openai": {"models": {
            "gpt-corpus": {"cost": {"input":1,"output":2,"cache_read":0.5,"cache_write":1.5}},
            "gpt-long": {"cost": {"input":1,"output":2,"context_over_200k":{"input":10,"output":20}}}
        }},
        "anthropic": {"models": {
            "claude-corpus": {"cost": {"input":5,"output":10,"cache_read":1,"cache_write":2,
                "tiers":[{"tier":{"type":"context","size":100},"input":8,"output":12,"cache_read":2,"cache_write":3}]}}
        }}
    }});
    let prices = vec![
        json!({"provider":"openai","model":"gpt-corpus","usage":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":10,"outputTokens":5,"reasoningOutputTokens":2},"context":100,"uncached":false}),
        json!({"provider":"anthropic","model":"claude-corpus","usage":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":10,"outputTokens":5},"context":100,"uncached":false}),
        json!({"provider":"openai","model":"gpt-long","usage":{"inputTokens":100,"outputTokens":10},"context":200000,"uncached":false}),
        json!({"provider":"openai","model":"gpt-long","usage":{"inputTokens":100,"outputTokens":10},"context":200001,"uncached":false}),
        json!({"provider":"openai","model":"gpt-long","usage":{"inputTokens":100,"cachedInputTokens":0,"outputTokens":10},"context":null,"uncached":true}),
    ];
    let records = vec![
        (
            1,
            Some("a".into()),
            Some("th".into()),
            Some("turn".into()),
            1.0,
            json!({"agentId":"a","threadId":"th","turnId":"turn","model":"gpt-corpus","delta":{"inputTokens":9,"outputTokens":1}}),
        ),
        (
            2,
            Some("a".into()),
            Some("th".into()),
            Some("turn".into()),
            2.0,
            json!({"agentId":"a","threadId":"th","turnId":"turn","model":"gpt-corpus","responseId":"r","last":{"inputTokens":7,"cachedInputTokens":2,"outputTokens":1}}),
        ),
        (
            3,
            Some("b".into()),
            Some("th".into()),
            Some("turn".into()),
            3.0,
            json!({"agentId":"b","threadId":"th","turnId":"turn","model":"gpt-corpus","accountKey":"alt","responseId":false,"last":{"inputTokens":4,"outputTokens":2}}),
        ),
        (
            4,
            Some("b".into()),
            Some("th".into()),
            Some("next".into()),
            4.0,
            json!({"agentId":"b","threadId":"th","turnId":"next","accountKey":"alt","responseId":[],"last":{"inputTokens":3,"outputTokens":1}}),
        ),
        (
            5,
            Some("b".into()),
            Some("th".into()),
            Some("turn".into()),
            5.0,
            json!({"agentId":"b","threadId":"th","turnId":"turn","accountKey":"alt","responseId":"current","last":{"inputTokens":6,"outputTokens":2}}),
        ),
    ];
    let snapshot = CostUsageSnapshot::from_records(records.clone()).expect("valid corpus rows");
    let rust_groups = snapshot.groups().iter().map(|g| json!({
        "model":g.model,"accountKey":g.account_key,"inputTokens":g.input_tokens,
        "cachedInputTokens":g.cached_tokens,"cacheWriteInputTokens":g.write_tokens,
        "outputTokens":g.output_tokens,"inputTokensAreUncached":g.input_uncached,"count":g.count
    })).collect::<Vec<_>>();
    let input = json!({"catalog":catalog,"prices":prices,"records":records.iter().map(|(seq,a,t,turn,at,row)|json!({
        "sequence":seq,"agent":a,"thread":t,"turn":turn,"at":at,"record":row
    })).collect::<Vec<_>>()});
    let source = server_source();
    let script = r#"
import json, os, sqlite3, sys, tempfile
from pathlib import Path
sys.path.insert(0, os.environ['COSTS_TEST_DIR'])
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, os.environ['COSTS_SERVER_SRC'])
from codex_pricing import price_usage
from codex_session_costs import SessionCostReader
from codex_token_rate import TokenRates
payload=json.load(sys.stdin)
prices=[]
for c in payload['prices']:
    prices.append(price_usage(payload['catalog'],c['provider'],c['model'],c['usage'],context_tokens=c['context'],input_tokens_are_uncached=c['uncached']))
with tempfile.TemporaryDirectory(prefix='costs-oracle-') as directory:
    db=sqlite3.connect(':memory:')
    db.execute('CREATE TABLE analytics_usage(seq INTEGER,id TEXT,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT)')
    db.execute('CREATE TABLE session_cost_claude_messages(account_key TEXT,thread_id TEXT,response_id TEXT)')
    for row in payload['records']:
        db.execute('INSERT INTO analytics_usage VALUES(?,?,?,?,?,?,?,?)',(row['sequence'],str(row['sequence']),row['agent'][0], 'root',row['thread'][0],row['turn'][0],row['at'],json.dumps(row['record'])))
    class NoPricing:
        def snapshot(self): return None
    reader=SessionCostReader(Path(directory)/'analytics.sqlite3',NoPricing(),state_root=Path(directory))
    groups=[[row[0],row[1],*[float(value) if value is not None else None for value in row[2:6]],bool(row[6]),row[7]] for row in reader._cost_usage_groups(db,'root')]
    valid={'rootId':'root','pricingState':'ready','unknownModels':[],'breakdown':{'providers':{},'models':{}},'totalUSD':1,'pricedSamples':1}
    cache=[reader._valid_cached('root',None),reader._valid_cached('root',{**valid,'rootId':'stale'}),reader._valid_cached('root',valid)]
    db.close()
clock=[10.0]
rates=TokenRates(clock=lambda:clock[0])
agent={'id':'agent-a','rootId':'root','threadId':'thread-a','inFlight':False,'turnId':'turn-a'}
rates.observe(agent,'turn/started',{'turnId':'turn-a'},'account','connection',at=10.0)
clock[0]=12.0
rates.observe(agent,'thread/tokenUsage/updated',{'turnId':'turn-a','tokenUsage':{'total':{'outputTokens':5},'last':{'outputTokens':5}}},'account','connection',at=12.0)
print(json.dumps({'prices':prices,'groups':groups,'cache':cache,'rates':rates.workspace_snapshot()},sort_keys=True))
"#;
    let mut child = Command::new(python())
        .arg("-B")
        .arg("-c")
        .arg(script)
        .env(
            "COSTS_TEST_DIR",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle"),
        )
        .env("COSTS_SERVER_SRC", &source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start isolated Python oracle");
    serde_json::to_writer(child.stdin.as_mut().expect("oracle stdin is piped"), &input)
        .expect("write differential corpus to Python");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    let catalog = PricingCatalog::from_value(&input["catalog"]).expect("pricing corpus catalog");
    let rust_prices = prices
        .iter()
        .map(|case| {
            let priced = catalog.price_usage(
                case["provider"].as_str().expect("corpus provider"),
                case["model"].as_str().expect("corpus model"),
                &case["usage"],
                case["context"].as_f64(),
                case["uncached"].as_bool().expect("uncached flag"),
            );
            json!([
                priced.amount_usd,
                format!("{:?}", priced.status).to_lowercase(),
                priced.context_tier_used
            ])
        })
        .collect::<Vec<_>>();
    assert_eq!(
        json!(rust_prices),
        oracle["prices"],
        "pricing oracle differs"
    );
    assert_eq!(oracle["cache"], json!([false, false, true]));
    let rate_snapshot = TokenRateSnapshot::from_value(oracle["rates"].clone())
        .expect("Python token-rate aggregation output is valid");
    assert_eq!(rate_snapshot.to_value(), oracle["rates"]);
    // Python's cost query tuple order is the GROUP_COLUMNS contract.
    let mut oracle_groups = oracle["groups"].as_array().expect("oracle groups").iter().map(|row| json!({
        "model":row[0],"accountKey":row[1],"inputTokens":row[2],"cachedInputTokens":row[3],
        "cacheWriteInputTokens":row[4],"outputTokens":row[5],"inputTokensAreUncached":row[6].as_i64()==Some(1),"count":row[7]
    })).collect::<Vec<_>>();
    let mut rust_groups = rust_groups;
    let sort = |rows: &mut Vec<Value>| {
        rows.sort_by_key(|row| serde_json::to_string(row).expect("serializable corpus row"))
    };
    sort(&mut oracle_groups);
    sort(&mut rust_groups);
    assert_eq!(rust_groups, oracle_groups, "cost grouping oracle differs");
}
