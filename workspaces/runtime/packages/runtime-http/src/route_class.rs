//! The one place that classifies API routes for the permission table
//! (`studio_runtime_services::principal`). A safe method is a read. Every other
//! operation of the OpenAPI document must have a row here; the test below fails for a
//! new route without a class.

use studio_runtime_services::principal::CommandClass::{
    self, ChatControl, Federation, Host, OwnerOnly, Read, Spawn,
};

/// `(method, OpenAPI path template, class)` of every non-safe operation.
const ROUTES: &[(&str, &str, CommandClass)] = &[
    ("POST", "/api/accounts/claude/add", OwnerOnly),
    ("POST", "/api/accounts/claude/login", OwnerOnly),
    ("POST", "/api/accounts/claude/login/cancel", OwnerOnly),
    ("POST", "/api/accounts/claude/login/code", OwnerOnly),
    ("POST", "/api/accounts/default", OwnerOnly),
    ("POST", "/api/accounts/default-groups", OwnerOnly),
    ("POST", "/api/accounts/delete", OwnerOnly),
    ("POST", "/api/accounts/disconnect", OwnerOnly),
    ("POST", "/api/accounts/discover", OwnerOnly),
    ("POST", "/api/accounts/group", OwnerOnly),
    ("POST", "/api/accounts/login", OwnerOnly),
    ("POST", "/api/accounts/login/cancel", OwnerOnly),
    ("POST", "/api/accounts/name", OwnerOnly),
    ("POST", "/api/accounts/reconnect", OwnerOnly),
    ("POST", "/api/accounts/register", OwnerOnly),
    ("POST", "/api/accounts/throttle", OwnerOnly),
    ("POST", "/api/action", ChatControl),
    ("POST", "/api/agents", Spawn),
    ("POST", "/api/agents/account", ChatControl),
    ("POST", "/api/agents/account-transfer", ChatControl),
    ("POST", "/api/agents/move-to-host", Host),
    ("POST", "/api/agents/workspace-mode", Host),
    ("POST", "/api/annotation", ChatControl),
    ("POST", "/api/answer", ChatControl),
    ("POST", "/api/assets", Host),
    ("POST", "/api/branch", Host),
    ("POST", "/api/browser/forward", Host),
    ("POST", "/api/capacity-retry", ChatControl),
    ("POST", "/api/checkpoint", Host),
    ("POST", "/api/checkpoint/preview", Read),
    ("POST", "/api/checkpoint/restore", Host),
    ("POST", "/api/claude/file-backups", Host),
    ("POST", "/api/claude/profiles", OwnerOnly),
    ("POST", "/api/claude/session", ChatControl),
<<<<<<< head
    ("POST", "/api/commands", ChatControl),
||||||| base
=======
>>>>>>> origin/feat/principal-permissions
    ("POST", "/api/complaints", ChatControl),
    ("POST", "/api/configure", OwnerOnly),
    ("POST", "/api/connection-recovery", ChatControl),
    ("POST", "/api/conversation", ChatControl),
    ("POST", "/api/conversation/delete", ChatControl),
    ("POST", "/api/costs/interest", ChatControl),
    ("POST", "/api/federation", OwnerOnly),
    ("POST", "/api/federation/v1/message", Federation),
    ("POST", "/api/federation/v1/pair", Federation),
    ("POST", "/api/federation/v1/pull", Federation),
    ("POST", "/api/federation/v1/status", Federation),
    ("POST", "/api/leads", Spawn),
    ("POST", "/api/limits/reset", ChatControl),
    ("POST", "/api/linux-vm/settings", OwnerOnly),
    ("POST", "/api/mac-sync/config", OwnerOnly),
    ("POST", "/api/mac-sync/resolve", OwnerOnly),
    ("POST", "/api/mcp/reconnect", Host),
    ("POST", "/api/messages", ChatControl),
    ("POST", "/api/monitor/input", Host),
    ("POST", "/api/monitor/restart", Host),
    ("POST", "/api/multi-server", OwnerOnly),
    ("POST", "/api/multi-server/v1/auto-pair", OwnerOnly),
    ("POST", "/api/multi-server/v1/name", OwnerOnly),
    ("POST", "/api/multi-server/v1/pair", OwnerOnly),
    ("POST", "/api/native-command", Host),
    ("POST", "/api/orchestration/settings", OwnerOnly),
    ("POST", "/api/organization", OwnerOnly),
    ("POST", "/api/peer-teams", Spawn),
    ("POST", "/api/processes/action", Host),
    ("POST", "/api/profiles", OwnerOnly),
    ("POST", "/api/project-secrets/list", Read),
    ("POST", "/api/project-secrets/remove", Host),
    ("POST", "/api/project-secrets/set", Host),
    ("POST", "/api/projects", Host),
    ("POST", "/api/projects/delete", Host),
    ("POST", "/api/questions/defer", ChatControl),
    ("POST", "/api/questions/delete", ChatControl),
    ("POST", "/api/queue", ChatControl),
    ("POST", "/api/rename", ChatControl),
    ("POST", "/api/room/delete", ChatControl),
    ("POST", "/api/rules", OwnerOnly),
    ("POST", "/api/servers/orchestration", OwnerOnly),
    ("POST", "/api/stop", ChatControl),
    ("POST", "/api/studio-errors/dismiss", ChatControl),
    ("POST", "/api/sync/drafts", ChatControl),
    ("POST", "/api/sync/notify", ChatControl),
    ("POST", "/api/sync/preferences", ChatControl),
    ("POST", "/api/sync/subscriptions", Read),
    ("POST", "/api/terminals/close", Host),
    ("POST", "/api/terminals/create", Host),
    ("POST", "/api/terminals/input", Host),
    ("POST", "/api/terminals/rename", Host),
    ("POST", "/api/terminals/resize", Host),
    ("POST", "/api/usage-resume", ChatControl),
    ("POST", "/api/vault/aws-login", OwnerOnly),
    ("POST", "/api/vault/pull", OwnerOnly),
    ("POST", "/api/vault/setup", OwnerOnly),
    ("POST", "/api/vault/setup/facts", OwnerOnly),
    ("POST", "/api/vault/unlock", OwnerOnly),
    ("POST", "/api/voice/end", ChatControl),
    ("POST", "/api/voice/records", Read),
    ("POST", "/api/voice/start", ChatControl),
    ("POST", "/api/voice/status", Read),
    ("POST", "/api/web-push/remove", ChatControl),
    ("POST", "/api/web-push/subscriptions", ChatControl),
    ("POST", "/api/work", ChatControl),
];

/// The class of an operation, given its method and OpenAPI path template. A write
/// without a row has no class; the caller treats it as owner only.
pub(crate) fn class_of(method: &str, template: &str) -> Option<CommandClass> {
    if matches!(method, "GET" | "HEAD" | "OPTIONS") {
        return Some(Read);
    }
    ROUTES
        .iter()
        .find(|(m, t, _)| *m == method && *t == template)
        .map(|(_, _, class)| *class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_route_has_a_class() {
        let operations = crate::schema::operations().expect("OpenAPI operations load");
        let missing = operations
            .iter()
            .filter(|(method, template)| class_of(method, template).is_none())
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "classify these routes in route_class.rs: {missing:?}"
        );
    }

    #[test]
    fn no_row_names_a_route_that_does_not_exist() {
        let operations = crate::schema::operations().expect("OpenAPI operations load");
        let stale = ROUTES
            .iter()
            .filter(|(m, t, _)| !operations.contains(&((*m).to_owned(), (*t).to_owned())))
            .collect::<Vec<_>>();
        assert!(stale.is_empty(), "remove from route_class.rs: {stale:?}");
    }
}
