//! The one permission table. The edge checks a token or a signature and builds one
//! `Principal`. `access` maps a principal and a command class to an answer without I/O.
//! A `Scoped` answer names the row check that the caller still owes; the code that holds
//! the rows (agent tree, link workers, rooms) makes that check.

/// What a paired device may do. Read from the server's own trust list, never from a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Full,
    Observe,
}

impl Grant {
    /// A trust-list record without a `grant` field is `Full` (the default at pairing).
    pub fn from_record(record: &serde_json::Value) -> Self {
        match record.get("grant").and_then(serde_json::Value::as_str) {
            Some("observe") => Self::Observe,
            _ => Self::Full,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Loopback owner token, or a Tailscale Serve request from a tailnet address.
    Owner,
    /// A paired phone or laptop that signs its requests.
    Device {
        id: String,
        grant: Grant,
    },
    /// A paired server.
    Peer {
        server: String,
    },
    /// A managed agent through the agent tools. Its tree position comes from the rows.
    Agent {
        id: String,
        epoch: u64,
    },
    Federated {
        peer: String,
    },
    /// Automatic answers.
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    Read,
    /// Send, stop, answer, withdraw.
    ChatControl,
    Spawn,
    /// Exec, git, folders, file write.
    Host,
    /// Pairing, grants, revoke, settings.
    OwnerOnly,
    Federation,
}

/// The row check that a `Scoped` answer leaves to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The target is a descendant of the agent (`is_descendant`).
    Descendant,
    /// The target is a worker of the peer's link.
    LinkWorker,
    /// The new agent belongs to the peer's own link.
    OwnLink,
    /// The caller is a root lead.
    RootLead,
    /// The room is approved for the peer.
    ApprovedRoom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Allow,
    Deny,
    Scoped(Scope),
}

impl Access {
    /// Edge decision: a scoped answer passes here and is checked where the rows are.
    pub fn passes_edge(self) -> bool {
        !matches!(self, Self::Deny)
    }
}

pub fn access(principal: &Principal, class: CommandClass) -> Access {
    use Access::{Allow, Deny, Scoped};
    use CommandClass::{ChatControl, Federation, Host, OwnerOnly, Read, Spawn};
    use Principal::{Agent, Device, Federated, Owner, Peer, Policy};
    match (principal, class) {
        (_, Read) => Allow,
        (Owner, Federation) => Deny,
        (Owner, _) => Allow,
        (
            Device {
                grant: Grant::Full, ..
            },
            OwnerOnly | Federation,
        ) => Deny,
        (
            Device {
                grant: Grant::Full, ..
            },
            _,
        ) => Allow,
        (
            Device {
                grant: Grant::Observe,
                ..
            },
            _,
        ) => Deny,
        (Peer { .. }, ChatControl) => Scoped(Scope::LinkWorker),
        (Peer { .. }, Spawn) => Scoped(Scope::OwnLink),
        (Peer { .. }, Host) => Allow,
        (Agent { .. }, ChatControl) => Scoped(Scope::Descendant),
        (Agent { .. }, Spawn) => Scoped(Scope::RootLead),
        (Federated { .. }, Federation) => Scoped(Scope::ApprovedRoom),
        (Policy, ChatControl) => Allow,
        _ => Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Access::{Allow, Deny, Scoped};

    fn device(grant: Grant) -> Principal {
        Principal::Device {
            id: "d".into(),
            grant,
        }
    }

    #[test]
    fn table_covers_every_class_for_every_principal() {
        use CommandClass::{ChatControl, Federation, Host, OwnerOnly, Read, Spawn};
        let rows: Vec<(Principal, [Access; 6])> = vec![
            //                                  Read, Chat, Spawn, Host, OwnerOnly, Federation
            (Principal::Owner, [Allow, Allow, Allow, Allow, Allow, Deny]),
            (
                device(Grant::Full),
                [Allow, Allow, Allow, Allow, Deny, Deny],
            ),
            (
                device(Grant::Observe),
                [Allow, Deny, Deny, Deny, Deny, Deny],
            ),
            (
                Principal::Peer { server: "s".into() },
                [
                    Allow,
                    Scoped(Scope::LinkWorker),
                    Scoped(Scope::OwnLink),
                    Allow,
                    Deny,
                    Deny,
                ],
            ),
            (
                Principal::Agent {
                    id: "a".into(),
                    epoch: 1,
                },
                [
                    Allow,
                    Scoped(Scope::Descendant),
                    Scoped(Scope::RootLead),
                    Deny,
                    Deny,
                    Deny,
                ],
            ),
            (
                Principal::Federated { peer: "p".into() },
                [Allow, Deny, Deny, Deny, Deny, Scoped(Scope::ApprovedRoom)],
            ),
            (Principal::Policy, [Allow, Allow, Deny, Deny, Deny, Deny]),
        ];
        for (principal, expected) in rows {
            let classes = [Read, ChatControl, Spawn, Host, OwnerOnly, Federation];
            for (class, want) in classes.into_iter().zip(expected) {
                assert_eq!(access(&principal, class), want, "{principal:?} {class:?}");
            }
        }
    }

    #[test]
    fn grant_comes_from_the_record_and_defaults_to_full() {
        assert_eq!(Grant::from_record(&serde_json::json!({})), Grant::Full);
        assert_eq!(
            Grant::from_record(&serde_json::json!({"grant":"observe"})),
            Grant::Observe
        );
        assert_eq!(
            Grant::from_record(&serde_json::json!({"grant":"bogus"})),
            Grant::Full
        );
    }
}
