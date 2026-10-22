//! Resolve worker limits before creation and enforce their durable turn budget.

use serde_json::{Value, json};
use studio_runtime_schema::worker_capabilities::WorkerCapabilities;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, StoreResult, Transaction};

pub(crate) use studio_runtime_schema::worker_capabilities::TURN_LIMIT_REASON;
pub(crate) const TURN_LIMIT_MESSAGE: &str =
    "The worker reached maxTurns. Send an explicit lead instruction to reset the count.";

pub(crate) fn from_value(value: &Value) -> Result<WorkerCapabilities, &'static str> {
    let caps: WorkerCapabilities =
        serde_json::from_value(value.clone()).map_err(|_| "Invalid worker capability profile")?;
    caps.validate()
        .map_err(|_| "Invalid worker capability profile")?;
    Ok(caps)
}

pub(crate) fn from_record(value: &Value) -> Result<WorkerCapabilities, &'static str> {
    value
        .get("capabilityProfile")
        .map(from_value)
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Conservative inclusion: equal patterns, the universal wildcard, or a literal
/// matched by a parent pattern. Unknown pattern inclusion is never permission.
fn covered(child: &str, parent: &str) -> bool {
    if child == parent || parent == "*" {
        return true;
    }
    if child.contains('*') {
        return false;
    }
    let mut previous = vec![false; child.len() + 1];
    previous[0] = true;
    for p in parent.bytes() {
        let mut next = vec![false; child.len() + 1];
        next[0] = p == b'*' && previous[0];
        for (i, c) in child.bytes().enumerate() {
            next[i + 1] = if p == b'*' {
                previous[i + 1] || next[i]
            } else {
                previous[i] && p == c
            };
        }
        previous = next;
    }
    previous[child.len()]
}

pub(crate) fn narrow(
    base: &WorkerCapabilities,
    child: &WorkerCapabilities,
) -> Result<WorkerCapabilities, &'static str> {
    base.validate().map_err(|_| "Invalid worker profile")?;
    child
        .validate()
        .map_err(|_| "Invalid worker capability override")?;
    for (field, outer, inner, patterns) in [
        (
            "allowedTools override widens the worker profile",
            &base.allowed_tools,
            &child.allowed_tools,
            true,
        ),
        (
            "mcpServers override widens the worker profile",
            &base.mcp_servers,
            &child.mcp_servers,
            false,
        ),
        (
            "skills override widens the worker profile",
            &base.skills,
            &child.skills,
            false,
        ),
    ] {
        if let (Some(outer), Some(inner)) = (outer, inner)
            && inner.iter().any(|name| {
                !outer.iter().any(|limit| {
                    if patterns {
                        covered(name, limit)
                    } else {
                        name == limit
                    }
                })
            })
        {
            return Err(field);
        }
    }
    if let (Some(outer), Some(inner)) = (base.max_turns, child.max_turns)
        && inner > outer
    {
        return Err("maxTurns override widens the worker profile");
    }
    let mut denied = base.disallowed_tools.clone();
    if let Some(extra) = &child.disallowed_tools {
        let denied = denied.get_or_insert_with(Vec::new);
        for pattern in extra {
            if !denied.contains(pattern) {
                denied.push(pattern.clone());
            }
        }
    }
    Ok(WorkerCapabilities {
        allowed_tools: child
            .allowed_tools
            .clone()
            .or_else(|| base.allowed_tools.clone()),
        disallowed_tools: denied,
        mcp_servers: child
            .mcp_servers
            .clone()
            .or_else(|| base.mcp_servers.clone()),
        skills: child.skills.clone().or_else(|| base.skills.clone()),
        max_turns: child.max_turns.or(base.max_turns),
    })
}

pub(crate) fn for_provider(caps: &WorkerCapabilities, provider: &str) -> Result<(), &'static str> {
    if provider == "codex" {
        for (configured, message) in [
            (
                caps.allowed_tools.is_some(),
                "Codex does not support the allowedTools profile field",
            ),
            (
                caps.disallowed_tools.is_some(),
                "Codex does not support the disallowedTools profile field",
            ),
            (
                caps.skills.is_some(),
                "Codex does not support the skills profile field",
            ),
        ] {
            if configured {
                return Err(message);
            }
        }
    } else if provider != "claude" {
        return Err("Worker capability profiles require a supported provider");
    }
    Ok(())
}

pub(crate) fn exhausted(agent: &Value) -> bool {
    agent["capabilityProfile"]["maxTurns"]
        .as_u64()
        .is_some_and(|cap| agent["completedWorkerTurns"].as_u64().unwrap_or(0) >= cap)
}

pub(crate) fn reset_on_lead_instruction(agent: &mut AgentRecord) {
    agent.extra.insert("completedWorkerTurns".into(), json!(0));
    agent.extra.remove("terminalReason");
}

/// Share counting with native disconnect recovery. The turn marker survives a
/// lead reset, so a repeated completion cannot spend the new allowance.
pub(crate) fn count_completed(agent: &mut AgentRecord, turn_id: &str) {
    if !agent.extra.get("parentId").is_some_and(Value::is_string) {
        return;
    }
    let identity = json!({"threadId":agent.extra.get("threadId"),"turnId":turn_id});
    if agent.extra.get("countedWorkerTurn") == Some(&identity) {
        return;
    }
    agent.extra.insert("countedWorkerTurn".into(), identity);
    let count = agent
        .extra
        .get("completedWorkerTurns")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    agent
        .extra
        .insert("completedWorkerTurns".into(), json!(count));
    if exhausted(&agent.value()) {
        agent.extra.insert("autoWake".into(), json!(false));
        agent
            .extra
            .insert("terminalReason".into(), json!(TURN_LIMIT_REASON));
        agent
            .extra
            .insert("error".into(), json!(TURN_LIMIT_MESSAGE));
        agent.status = studio_runtime_store::AgentStatus::Completed;
    }
}

/// Called inside the terminal receipt transaction, before parent delivery.
pub(crate) fn completed_turn(tx: &Transaction<'_>, id: &str, turn_id: &str) -> StoreResult<()> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return Ok(());
    };
    count_completed(&mut agent, turn_id);
    AgentRepository::put(tx, &agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_overrides_only_narrow_all_fields() {
        let base = from_value(&json!({"allowedTools":["Read*"],"disallowedTools":["Bash"],"mcpServers":["docs"],"skills":["research"],"maxTurns":3})).unwrap();
        let child = from_value(&json!({"allowedTools":["Read"],"disallowedTools":[],"mcpServers":[],"skills":[],"maxTurns":1})).unwrap();
        let effective = narrow(&base, &child).unwrap();
        assert_eq!(effective.disallowed_tools, Some(vec!["Bash".into()]));
        assert_eq!(effective.max_turns, Some(1));
        for (field, value) in [
            ("allowedTools", json!(["Write"])),
            ("mcpServers", json!(["other"])),
            ("skills", json!(["other"])),
            ("maxTurns", json!(4)),
        ] {
            let widened = from_value(&json!({field:value})).unwrap();
            assert!(narrow(&base, &widened).unwrap_err().contains(field));
        }
        assert_eq!(narrow(&base, &WorkerCapabilities::default()).unwrap(), base);
    }

    #[test]
    fn capability_provider_refuses_unsupported_codex_fields() {
        for field in ["allowedTools", "disallowedTools", "skills"] {
            let caps = from_value(&json!({field:[]})).unwrap();
            assert!(for_provider(&caps, "codex").unwrap_err().contains(field));
            for_provider(&caps, "claude").unwrap();
        }
        for_provider(
            &from_value(&json!({"mcpServers":["docs"],"maxTurns":1})).unwrap(),
            "codex",
        )
        .unwrap();
    }
}
