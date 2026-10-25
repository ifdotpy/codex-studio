//! Fields of the removed team token budget. The frozen Python oracles still carry
//! them; current builds neither write nor publish them, so the corpus comparisons
//! drop them from both sides.
use serde_json::Value;

const FIELDS: [&str; 4] = [
    "tokenBudget",
    "tokensUsed",
    "tokenUsageAccounting",
    "budgetBlocked",
];

/// Remove the budget fields from every object in `value`.
pub(crate) fn strip(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for field in FIELDS {
                object.remove(field);
            }
            object.values_mut().for_each(strip);
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}
