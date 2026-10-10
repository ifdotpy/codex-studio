//! Who may do what (docs/design.md, section 3): roles per project and machine, deny rules that
//! always win, and protection rules for lines. Root may do everything.
//!
//! The policy lives in three settings of the project (machine-local, signed records):
//! - `access`: principal to role, for example `{"user:1001": "admin", "group:2000": "writer"}`;
//! - `deny`: principal to the actions it may not take, for example `{"group:2000": ["push"]}`;
//! - `protect`: line name pattern to its rule, for example `{"main": {}}`.
//!
//! A principal is `user:<uid>`, `group:<gid>` or `*` (everyone).

use crate::store::Project;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    None,
    Reader,
    Writer,
    Maintainer,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "none" => Role::None,
            "reader" => Role::Reader,
            "writer" => Role::Writer,
            "maintainer" => Role::Maintainer,
            "admin" => Role::Admin,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::None => "none",
            Role::Reader => "reader",
            Role::Writer => "writer",
            Role::Maintainer => "maintainer",
            Role::Admin => "admin",
        }
    }
}

/// The actions of a project and the lowest role that takes each one. Changes inside a line are
/// not here: they belong to the owner of the line and to the line's protection rule.
pub const ACTIONS: &[(&str, Role)] = &[
    ("read", Role::Reader),
    ("try", Role::Reader),
    ("export", Role::Reader),
    ("line.create", Role::Writer),
    ("merge", Role::Writer),
    ("line.manage", Role::Maintainer),
    ("tag", Role::Maintainer),
    ("push", Role::Maintainer),
    ("access", Role::Admin),
    ("config", Role::Admin),
    ("remote", Role::Admin),
    ("sync", Role::Admin),
    ("backup", Role::Admin),
    ("gc", Role::Admin),
    ("records", Role::Admin),
    ("fsck", Role::Admin),
];

pub fn action_role(action: &str) -> Option<Role> {
    ACTIONS.iter().find(|(a, _)| *a == action).map(|(_, r)| *r)
}

/// A protection rule of a line name pattern (as stored).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub check: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,
    /// Paths (a folder, a file or a glob) whose change needs a higher role than `update`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub paths: BTreeMap<String, String>,
    /// Approvals of the merged state that a merge needs (`layr approve`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<u32>,
    /// Who may approve: principals (an approver matches all of them); none: a maintainer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<String>,
}

/// The rule that applies to a line: every matching pattern combined, the most restrictive value
/// of each setting.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub update: Role,
    pub expect: bool,
    pub check: Vec<String>,
    pub direct: bool,
    pub rewrite: Role,
    pub paths: BTreeMap<String, Role>,
    pub approvals: u32,
    pub approvers: Vec<String>,
}

impl Rule {
    /// The role a merge needs that changes `path`: `update`, or higher for a path rule.
    pub fn role_for_path(&self, path: &str) -> Role {
        self.paths.iter().filter(|(pat, _)| path_matches(pat, path)).map(|(_, r)| *r).fold(self.update, Role::max)
    }
}

/// A path rule matches the path itself, everything below a folder, or a glob.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let p = pattern.trim_end_matches('/');
    path == p || path.starts_with(&format!("{p}/")) || crate::cmd::glob(p, path)
}

fn role_or(s: &Option<String>, default: Role) -> Role {
    // An unknown role name is the most restrictive one: admin.
    s.as_deref().map(|x| Role::parse(x).unwrap_or(Role::Admin)).unwrap_or(default)
}

impl RuleSpec {
    fn rule(&self) -> Rule {
        Rule {
            update: role_or(&self.update, Role::Maintainer),
            expect: self.expect.unwrap_or(true),
            check: self.check.clone(),
            direct: self.direct.unwrap_or(false),
            rewrite: role_or(&self.rewrite, Role::Admin),
            paths: self.paths.iter().map(|(p, r)| (p.clone(), Role::parse(r).unwrap_or(Role::Admin))).collect(),
            approvals: self.approvals.unwrap_or(0),
            approvers: self.approvers.clone(),
        }
    }
}

/// Who acts: a uid and its groups.
#[derive(Clone, Debug)]
pub struct Who {
    pub uid: u32,
    pub groups: Vec<u32>,
}

impl Who {
    pub fn of(uid: u32, gid: u32) -> Who {
        let mut groups = crate::privs::user(uid).map(|u| u.groups).unwrap_or_default();
        if !groups.contains(&gid) {
            groups.push(gid);
        }
        Who { uid, groups }
    }

    fn keys(&self) -> Vec<String> {
        let mut k = vec![format!("user:{}", self.uid)];
        k.extend(self.groups.iter().map(|g| format!("group:{g}")));
        k.push("*".into());
        k
    }
}

#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub access: BTreeMap<String, Role>,
    /// Roles given for a time (`grants`): principal to (role, end in ms since the epoch). Ended
    /// ones are not loaded. They add to the role in `access`, never replace it.
    pub grants: BTreeMap<String, (Role, i64)>,
    pub deny: BTreeMap<String, Vec<String>>,
    pub protect: BTreeMap<String, RuleSpec>,
}

impl Policy {
    pub fn load(p: &Project) -> Policy {
        let cfg = |k: &str| p.config_value(k).ok().flatten();
        let mut access = BTreeMap::new();
        let mut grants = BTreeMap::new();
        let now = crate::ids::now_ms();
        for (k, v) in cfg("grants").and_then(|v| v.as_object().cloned()).unwrap_or_default() {
            let end = v.get("until").and_then(|x| x.as_i64()).unwrap_or(0);
            let role = v.get("role").and_then(|r| r.as_str()).and_then(Role::parse).unwrap_or(Role::None);
            if end > now {
                grants.insert(k, (role, end));
            }
        }
        match cfg("access").and_then(|v| v.as_object().cloned()) {
            Some(m) => {
                for (k, v) in m {
                    // An unknown role name grants nothing.
                    access.insert(k, v.as_str().and_then(Role::parse).unwrap_or(Role::None));
                }
            }
            None => {
                // Stores of older versions: one admin (`admin`, before that `lead`) and the
                // members list (`*` or uids), who were writers.
                let admin = p.legacy_admin();
                if admin != 0 {
                    access.insert(format!("user:{admin}"), Role::Admin);
                }
                match cfg("members") {
                    None => {
                        access.insert("*".into(), Role::Writer);
                    }
                    Some(v) if v.as_str() == Some("*") => {
                        access.insert("*".into(), Role::Writer);
                    }
                    Some(v) => {
                        for u in v.as_array().into_iter().flatten().filter_map(|x| x.as_u64()) {
                            access.entry(format!("user:{u}")).or_insert(Role::Writer);
                        }
                    }
                }
            }
        }
        let deny = cfg("deny").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let protect = cfg("protect").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        Policy { access, grants, deny, protect }
    }

    /// The highest role of the user, its groups and everyone. Root is admin.
    pub fn role(&self, who: &Who) -> Role {
        if who.uid == 0 {
            return Role::Admin;
        }
        who.keys()
            .iter()
            .flat_map(|k| [self.access.get(k).copied(), self.grants.get(k).map(|g| g.0)])
            .flatten()
            .max()
            .unwrap_or(Role::None)
    }

    /// The principal whose deny rule takes `action` away from `who`, if any. Never for root.
    pub fn denied_by(&self, who: &Who, action: &str) -> Option<String> {
        if who.uid == 0 {
            return None;
        }
        who.keys().into_iter().find(|k| self.deny.get(k).map(|v| v.iter().any(|a| a == action || a == "*")).unwrap_or(false))
    }

    /// Whether `who` may take `action`, with the reason when not.
    pub fn check(&self, who: &Who, action: &str) -> Result<(), String> {
        if let Some(k) = self.denied_by(who, action) {
            return Err(format!("a deny rule for {} takes '{action}' away", show_principal(&k)));
        }
        let need = action_role(action).unwrap_or(Role::Admin);
        let have = self.role(who);
        if have < need {
            return Err(format!("'{action}' needs the role {}; you are {}", need.name(), have.name()));
        }
        Ok(())
    }

    /// The rule of a line: all matching patterns, the most restrictive value of each setting.
    pub fn rule_for(&self, line: &str) -> Option<Rule> {
        let mut out: Option<Rule> = None;
        for (pat, spec) in &self.protect {
            if !(pat == line || crate::cmd::glob(pat, line)) {
                continue;
            }
            let r = spec.rule();
            out = Some(match out {
                None => r,
                Some(o) => Rule {
                    update: o.update.max(r.update),
                    expect: o.expect || r.expect,
                    check: {
                        let mut c = o.check;
                        for x in r.check {
                            if !c.contains(&x) {
                                c.push(x);
                            }
                        }
                        c
                    },
                    direct: o.direct && r.direct,
                    rewrite: o.rewrite.max(r.rewrite),
                    paths: {
                        let mut m = o.paths;
                        for (p, role) in r.paths {
                            let e = m.entry(p).or_insert(role);
                            *e = (*e).max(role);
                        }
                        m
                    },
                    approvals: o.approvals.max(r.approvals),
                    approvers: {
                        let mut v = o.approvers;
                        for x in r.approvers {
                            if !v.contains(&x) {
                                v.push(x);
                            }
                        }
                        v
                    },
                },
            });
        }
        out
    }

    /// Whether `who` may approve for a rule: it matches every listed principal, or with none
    /// listed it has the maintainer role. Not for the authors of the merged states.
    pub fn may_approve(&self, rule: &Rule, who: &Who) -> bool {
        if rule.approvers.is_empty() {
            return self.role(who) >= Role::Maintainer;
        }
        let keys = who.keys();
        rule.approvers.iter().all(|a| keys.contains(a))
    }

    /// The `access` and `grants` settings as stored.
    pub fn access_json(&self) -> serde_json::Value {
        serde_json::Value::Object(self.access.iter().map(|(k, r)| (k.clone(), serde_json::json!(r.name()))).collect())
    }
    pub fn grants_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.grants.iter().map(|(k, (r, end))| (k.clone(), serde_json::json!({"role": r.name(), "until": end}))).collect(),
        )
    }

    /// Principals that may read the project (role reader or more), for the git store's access
    /// list: (everyone, uids, gids). Roles for a time are left out: the list does not change
    /// when they end, and their users read through layr.
    pub fn readers(&self) -> (bool, Vec<u32>, Vec<u32>) {
        let mut all = false;
        let (mut users, mut groups) = (Vec::new(), Vec::new());
        for (k, r) in &self.access {
            if *r < Role::Reader || self.deny.get(k).map(|v| v.iter().any(|a| a == "read" || a == "*")).unwrap_or(false) {
                continue;
            }
            if k == "*" {
                all = true;
            } else if let Some(u) = k.strip_prefix("user:").and_then(|x| x.parse().ok()) {
                users.push(u);
            } else if let Some(g) = k.strip_prefix("group:").and_then(|x| x.parse().ok()) {
                groups.push(g);
            }
        }
        (all, users, groups)
    }
}

/// A principal as typed by a person: `alice`, `uid:1001`, `@devs`, `group:devs`, `gid:2000`
/// or `*`. Returns the stored form (`user:<uid>`, `group:<gid>`, `*`).
pub fn parse_principal(s: &str) -> Option<String> {
    if s == "*" {
        return Some("*".into());
    }
    if let Some(u) = s.strip_prefix("uid:").or_else(|| s.strip_prefix("user:")) {
        return match u.parse::<u32>() {
            Ok(n) => Some(format!("user:{n}")),
            Err(_) => crate::sys::user_by_name(u).map(|x| format!("user:{}", x.uid)),
        };
    }
    if let Some(g) = s.strip_prefix("gid:") {
        return g.parse::<u32>().ok().map(|n| format!("group:{n}"));
    }
    if let Some(g) = s.strip_prefix('@').or_else(|| s.strip_prefix("group:")) {
        return match g.parse::<u32>() {
            Ok(n) => Some(format!("group:{n}")),
            Err(_) => group_by_name(g).map(|n| format!("group:{n}")),
        };
    }
    crate::sys::user_by_name(s).map(|x| format!("user:{}", x.uid))
}

/// A readable name of a stored principal.
pub fn show_principal(k: &str) -> String {
    if let Some(u) = k.strip_prefix("user:").and_then(|x| x.parse::<u32>().ok()) {
        return crate::sys::user_by_uid(u).map(|x| x.name).unwrap_or_else(|| format!("uid:{u}"));
    }
    if let Some(g) = k.strip_prefix("group:").and_then(|x| x.parse::<u32>().ok()) {
        return group_name(g).map(|n| format!("@{n}")).unwrap_or_else(|| format!("gid:{g}"));
    }
    k.to_string()
}

fn groups_file() -> Vec<(String, u32)> {
    std::fs::read_to_string("/etc/group")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.first()?.to_string(), f.get(2)?.parse().ok()?))
        })
        .collect()
}

fn group_by_name(name: &str) -> Option<u32> {
    groups_file().into_iter().find(|(n, _)| n == name).map(|(_, g)| g)
}

fn group_name(gid: u32) -> Option<String> {
    groups_file().into_iter().find(|(_, g)| *g == gid).map(|(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(uid: u32, groups: &[u32]) -> Who {
        Who { uid, groups: groups.to_vec() }
    }

    #[test]
    fn roles_deny_and_rules() {
        let mut p = Policy::default();
        p.access.insert("user:10".into(), Role::Admin);
        p.access.insert("group:50".into(), Role::Writer);
        p.access.insert("*".into(), Role::Reader);
        p.deny.insert("group:50".into(), vec!["push".into()]);
        assert_eq!(p.role(&who(10, &[])), Role::Admin);
        assert_eq!(p.role(&who(11, &[50])), Role::Writer);
        assert_eq!(p.role(&who(12, &[])), Role::Reader);
        assert!(p.check(&who(11, &[50]), "line.create").is_ok());
        assert!(p.check(&who(12, &[]), "line.create").is_err());
        // A deny rule wins over any role, except root's.
        p.access.insert("user:11".into(), Role::Admin);
        assert!(p.check(&who(11, &[50]), "push").is_err());
        assert!(p.check(&who(0, &[50]), "push").is_ok());
        // The most restrictive value of each matching rule.
        p.protect.insert("main".into(), RuleSpec { check: vec!["unit".into()], ..Default::default() });
        p.protect.insert(
            "ma*".into(),
            RuleSpec { update: Some("admin".into()), direct: Some(true), check: vec!["lint".into()], ..Default::default() },
        );
        let r = p.rule_for("main").unwrap();
        assert_eq!(r.update, Role::Admin);
        assert!(!r.direct && r.expect);
        assert_eq!(r.check, vec!["lint".to_string(), "unit".to_string()]);
        assert_eq!(r.rewrite, Role::Admin);
        assert!(p.rule_for("topic").is_none());
        // Path rules: the highest role of all matching paths.
        p.protect.insert(
            "main".into(),
            RuleSpec {
                paths: [(".github".to_string(), "admin".to_string()), ("tests/*.sh".into(), "maintainer".into())].into(),
                ..Default::default()
            },
        );
        let r = p.rule_for("main").unwrap();
        assert_eq!(r.role_for_path(".github/workflows/ci.yml"), Role::Admin);
        assert_eq!(r.role_for_path("tests/a.sh"), Role::Admin.max(r.update));
        assert_eq!(r.role_for_path("src/x.rs"), r.update);
        assert!(!path_matches(".github", ".githubx/y"));
        // An unknown role name in a rule restricts most.
        p.protect.insert("x".into(), RuleSpec { update: Some("bogus".into()), ..Default::default() });
        assert_eq!(p.rule_for("x").unwrap().update, Role::Admin);
    }
}
