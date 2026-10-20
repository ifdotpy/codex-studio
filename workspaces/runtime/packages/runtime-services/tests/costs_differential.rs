#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use studio_runtime_services::pricing::PricingCatalog;
use studio_runtime_services::token_rate::TokenRateSnapshot;

fn server_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
}

fn python() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| prepared_python_support::prepared_python(&server_source()))
        .clone()
}

#[test]
fn pricing_and_token_rates_match_real_python_oracles() {
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
    let input = json!({"catalog":catalog,"prices":prices});
    let source = server_source();
    let script = r#"
import json, os, sys
sys.path.insert(0, os.environ['COSTS_TEST_DIR'])
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, os.environ['COSTS_SERVER_SRC'])
from codex_pricing import price_usage
from codex_token_rate import TokenRates
payload=json.load(sys.stdin)
prices=[]
for c in payload['prices']:
    prices.append(price_usage(payload['catalog'],c['provider'],c['model'],c['usage'],context_tokens=c['context'],input_tokens_are_uncached=c['uncached']))
clock=[10.0]
rates=TokenRates(clock=lambda:clock[0])
agent={'id':'agent-a','rootId':'root','threadId':'thread-a','inFlight':False,'turnId':'turn-a'}
rates.observe(agent,'turn/started',{'turnId':'turn-a'},'account','connection',at=10.0)
clock[0]=12.0
rates.observe(agent,'thread/tokenUsage/updated',{'turnId':'turn-a','tokenUsage':{'total':{'outputTokens':5},'last':{'outputTokens':5}}},'account','connection',at=12.0)
print(json.dumps({'prices':prices,'rates':rates.workspace_snapshot()},sort_keys=True))
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
                serde_json::from_value(case["usage"].clone()).expect("corpus usage"),
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
    let rate_snapshot = TokenRateSnapshot::from_value(oracle["rates"].clone())
        .expect("Python token-rate aggregation output is valid");
    assert_eq!(rate_snapshot.to_value(), oracle["rates"]);
}
