//! Typed ingress for the workspace usage-limits schema. Provider extensions remain opaque.
use crate::StoredJsonInteger;
use crate::json_object::JsonObject;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageLimitsResponse {
    pub account_key: String,
    pub at: Option<f64>,
    pub checked_at: Option<f64>,
    pub data: Option<JsonObject<UsageLimitsData>>,
    pub error: Option<String>,
    pub processed_at: Option<f64>,
    pub read_at: Option<f64>,
    #[serde(default, deserialize_with = "present_eligibility")]
    pub eligibility: Option<JsonObject<UsageEligibility>>,
    #[serde(rename = "_syncEntities")]
    pub sync_entities: Option<Vec<JsonObject<SyncEntity>>>,
    #[serde(rename = "_syncEntitiesAfter")]
    pub sync_entities_after: Option<StoredJsonInteger>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsData {
    pub account_id: Option<String>,
    pub ordinary_usage_allowed: Option<bool>,
    pub rate_limit_reset_credits: Option<JsonObject<RateLimitResetCredits>>,
    pub rate_limits: Option<JsonObject<RateLimitBucket>>,
    pub rate_limits_by_limit_id: Option<BTreeMap<String, JsonObject<RateLimitBucket>>>,
    pub signed_in: Option<bool>,
    pub source: Option<String>,
    pub status: Option<String>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitBucket {
    pub individual_limit: Option<JsonObject<RateLimitIndividualLimit>>,
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub plan_type: Option<String>,
    pub primary: Option<JsonObject<RateLimitWindow>>,
    pub rate_limit_reached_type: Option<String>,
    pub secondary: Option<JsonObject<RateLimitWindow>>,
    pub spend_control_reached: Option<bool>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimitIndividualLimit {
    pub remaining_percent: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    pub resets_at: Option<f64>,
    pub used_percent: Option<f64>,
    pub window_duration_mins: Option<f64>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCredits {
    pub available_count: Option<StoredJsonInteger>,
    #[serde(default)]
    pub credits: Vec<JsonObject<RateLimitResetCredit>>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitResetCredit {
    pub id: String,
    pub reset_type: String,
    pub status: String,
    pub expires_at: Option<f64>,
    pub granted_at: Option<f64>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncEntity {
    pub id: String,
    pub seq: StoredJsonInteger,
    pub payload: String,
    #[serde(rename = "_deleted")]
    pub deleted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageEligibility {
    pub account_key: String,
    pub agent_id: String,
    pub allowed: bool,
    pub buckets: Vec<JsonObject<EligibilityBucket>>,
    pub can_run: bool,
    pub checked_at: f64,
    pub fresh: bool,
    pub model: String,
    pub overage: Overage,
    pub provider: String,
    #[serde(deserialize_with = "required_nullable")]
    pub reset_at: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub valid_until: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EligibilityBucket {
    pub id: String,
    pub overage_allowed: bool,
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
pub enum Overage {
    Allowed,
    InUse,
    Rejected,
    Unavailable,
}
impl TryFrom<String> for Overage {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "allowed" => Ok(Self::Allowed),
            "in_use" => Ok(Self::InUse),
            "rejected" => Ok(Self::Rejected),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err("invalid overage"),
        }
    }
}

fn required_nullable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    Option::deserialize(deserializer)
}
fn present_eligibility<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<JsonObject<UsageEligibility>>, D::Error> {
    JsonObject::<UsageEligibility>::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_fields_track_schema_owner() {
        let schema: Value =
            serde_json::from_str(include_str!("schemas/workspace-usage-limits.schema.json"))
                .expect("schema");
        for (name, fields) in [
            (
                "UsageLimitsResponse",
                "_syncEntities _syncEntitiesAfter accountKey at checkedAt data eligibility error processedAt readAt",
            ),
            (
                "UsageLimitsData",
                "accountId ordinaryUsageAllowed rateLimitResetCredits rateLimits rateLimitsByLimitId signedIn source status",
            ),
            (
                "RateLimitBucket",
                "individualLimit limitId limitName planType primary rateLimitReachedType secondary spendControlReached",
            ),
            ("RateLimitIndividualLimit", "remainingPercent"),
            ("RateLimitWindow", "resetsAt usedPercent windowDurationMins"),
            ("RateLimitResetCredits", "availableCount credits"),
            (
                "RateLimitResetCredit",
                "expiresAt grantedAt id resetType status",
            ),
            ("SyncEntity", "_deleted id payload seq"),
            (
                "UsageEligibility",
                "accountKey agentId allowed buckets canRun checkedAt fresh model overage provider resetAt validUntil",
            ),
        ] {
            let actual = schema["$defs"][name]["properties"]
                .as_object()
                .expect("properties");
            let expected = fields
                .split_whitespace()
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                actual
                    .keys()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>(),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn typed_ingress_matches_usage_limits_schema() {
        let full = json!({"accountKey":"a", "at":1, "checkedAt":2, "processedAt":3, "readAt":4,
            "error":null, "data":{"accountId":null,"ordinaryUsageAllowed":true,"signedIn":true,"source":"provider","status":"ok",
                "rateLimits":{"limitId":"one","limitName":"One","planType":"plus","rateLimitReachedType":null,"spendControlReached":false,
                    "individualLimit":{"remainingPercent":5},"primary":{"usedPercent":2,"resetsAt":3,"windowDurationMins":4,"vendor":[true,null]},"secondary":null},
                "rateLimitsByLimitId":{"one":{"primary":{"usedPercent":2}}},
                "rateLimitResetCredits":{"availableCount":2,"credits":[{"id":"x","status":"available","resetType":"manual","grantedAt":2,"expiresAt":3,"vendor":{}}]},"extension":{"arbitrary":[null,true]}},
            "_syncEntities":[{"id":"x","seq":1,"payload":"{}","_deleted":false}],"_syncEntitiesAfter":1,
            "eligibility":{"accountKey":"a","agentId":"b","allowed":true,"buckets":[{"id":"x","overageAllowed":false}],"canRun":true,"checkedAt":1,"fresh":true,"model":"m","overage":"allowed","provider":"codex","resetAt":null,"validUntil":null}});
        let cases = [
            ("/data/rateLimits/primary/usedPercent", json!("2")),
            ("/data/rateLimits/primary", json!([2, 3, 4])),
            ("/data/rateLimits/individualLimit", json!({"extra":1})),
            ("/data/rateLimitsByLimitId/one", json!([])),
            ("/data/rateLimitResetCredits/availableCount", json!(1.5)),
            ("/data/rateLimitResetCredits/credits/0", json!({"id":"x"})),
            ("/_syncEntities/0/seq", json!(-1)),
            ("/eligibility/overage", json!({"allowed":null})),
            ("/eligibility", json!(null)),
            ("/data", json!([])),
            ("/at", json!("1")),
        ];
        let mut candidates = vec![
            full.clone(),
            json!({"accountKey":"a"}),
            json!({"accountKey":"a","data":null}),
            json!([]),
        ];
        for (pointer, value) in cases {
            let mut candidate = full.clone();
            *candidate.pointer_mut(pointer).expect("fixture path") = value;
            candidates.push(candidate);
        }
        for pointer in [
            "/accountKey",
            "/at",
            "/checkedAt",
            "/processedAt",
            "/readAt",
            "/error",
            "/data",
            "/_syncEntities",
            "/_syncEntitiesAfter",
            "/eligibility",
            "/data/accountId",
            "/data/ordinaryUsageAllowed",
            "/data/signedIn",
            "/data/source",
            "/data/status",
            "/data/rateLimits",
            "/data/rateLimitsByLimitId",
            "/data/rateLimitResetCredits",
            "/data/rateLimits/limitId",
            "/data/rateLimits/limitName",
            "/data/rateLimits/planType",
            "/data/rateLimits/rateLimitReachedType",
            "/data/rateLimits/spendControlReached",
            "/data/rateLimits/individualLimit",
            "/data/rateLimits/individualLimit/remainingPercent",
            "/data/rateLimits/primary",
            "/data/rateLimits/secondary",
            "/data/rateLimits/primary/resetsAt",
            "/data/rateLimits/primary/usedPercent",
            "/data/rateLimits/primary/windowDurationMins",
            "/data/rateLimitResetCredits/availableCount",
            "/data/rateLimitResetCredits/credits",
            "/data/rateLimitResetCredits/credits/0/id",
            "/data/rateLimitResetCredits/credits/0/status",
            "/data/rateLimitResetCredits/credits/0/resetType",
            "/data/rateLimitResetCredits/credits/0/expiresAt",
            "/data/rateLimitResetCredits/credits/0/grantedAt",
            "/_syncEntities/0/id",
            "/_syncEntities/0/seq",
            "/_syncEntities/0/payload",
            "/_syncEntities/0/_deleted",
            "/eligibility/accountKey",
            "/eligibility/agentId",
            "/eligibility/allowed",
            "/eligibility/buckets",
            "/eligibility/buckets/0/id",
            "/eligibility/buckets/0/overageAllowed",
            "/eligibility/canRun",
            "/eligibility/checkedAt",
            "/eligibility/fresh",
            "/eligibility/model",
            "/eligibility/overage",
            "/eligibility/provider",
            "/eligibility/resetAt",
            "/eligibility/validUntil",
        ] {
            for value in [
                json!(null),
                json!(true),
                json!(-1),
                json!(1.0),
                json!(1.5),
                json!("x"),
                json!([]),
                json!({}),
            ] {
                let mut candidate = full.clone();
                *candidate.pointer_mut(pointer).expect("fixture path") = value;
                candidates.push(candidate);
            }
        }
        for candidate in candidates {
            let schema_valid =
                crate::first_error("workspace-usage-limits/UsageLimitsResponse", &candidate)
                    .expect("schema exists")
                    .is_none();
            let typed_valid =
                serde_json::from_value::<JsonObject<UsageLimitsResponse>>(candidate.clone())
                    .is_ok();
            assert_eq!(typed_valid, schema_valid, "{candidate}");
        }
    }
}
