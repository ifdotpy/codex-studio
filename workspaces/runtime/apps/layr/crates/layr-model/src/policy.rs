//! Access policy from signed settings. User groups and time come from the caller.
use super::Project;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    ("gc", Role::Admin),
    ("records", Role::Admin),
    ("fsck", Role::Admin),
];

pub fn action_role(action: &str) -> Option<Role> {
    ACTIONS.iter().find(|(a, _)| *a == action).map(|(_, r)| *r)
}

/// A protection rule of a line or ref name pattern (as stored).
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

/// The rule that applies to a line or ref: every matching pattern combined, the most restrictive value
/// of each setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    path == p || path.starts_with(&format!("{p}/")) || glob(p, path)
}

fn role_or(s: &Option<String>, default: Role) -> Role {
    // An unknown role name is the most restrictive one: admin.
    s.as_deref().map(|x| Role::parse(x).unwrap_or(Role::Admin)).unwrap_or(default)
}

impl RuleSpec {
    pub fn rule(&self) -> Rule {
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Who {
    pub uid: u32,
    pub groups: Vec<u32>,
}

impl Who {
    fn keys(&self) -> Vec<String> {
        let mut k = vec![format!("user:{}", self.uid)];
        k.extend(self.groups.iter().map(|g| format!("group:{g}")));
        k.push("*".into());
        k
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Policy {
    pub access: BTreeMap<String, Role>,
    /// Roles given for a time (`grants`): principal to (role, end in ms since the epoch). Ended
    /// ones are not loaded. They add to the role in `access`, never replace it.
    pub grants: BTreeMap<String, (Role, i64)>,
    pub deny: BTreeMap<String, Vec<String>>,
    pub protect: BTreeMap<String, RuleSpec>,
}

impl Policy {
    pub fn from_project(project: &Project, now: i64, overlay: bool) -> Self {
        let cfg = |key: &str| {
            if overlay && matches!(key, "access" | "deny" | "overlay") {
                project.overlay_setting(key).cloned()
            } else {
                project.setting(key).cloned()
            }
        };
        let mut access = BTreeMap::new();
        let mut grants = BTreeMap::new();

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
            None if overlay => {}
            None => {
                // Stores of older versions: one admin (`admin`, before that `lead`) and the
                // members list (`*` or uids), who were writers.
                let admin = cfg("admin").and_then(|v| v.as_u64()).or_else(|| cfg("lead").and_then(|v| v.as_u64())).unwrap_or(0);
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

    /// The rule of a line or ref: all matching patterns, with the most restrictive values.
    pub fn rule_for(&self, line: &str) -> Option<Rule> {
        let mut out: Option<Rule> = None;
        for (pat, spec) in &self.protect {
            if !(pat == line || glob(pat, line)) {
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
}

fn show_principal(s: &str) -> String {
    s.to_string()
}

pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s) = (pat.as_bytes(), s.as_bytes());
    // One pattern item at p[i] against byte c: Some(length of the item) if it matches.
    let item = |i: usize, c: u8| -> Option<usize> {
        match p[i] {
            b'?' => Some(1),
            b'[' => match p[i + 1..].iter().position(|x| *x == b']') {
                Some(off) => {
                    let end = i + 1 + off;
                    let set = &p[i + 1..end];
                    let (neg, set) =
                        if set.first() == Some(&b'!') || set.first() == Some(&b'^') { (true, &set[1..]) } else { (false, set) };
                    let mut hit = false;
                    let mut k = 0;
                    while k < set.len() {
                        if k + 2 < set.len() && set[k + 1] == b'-' {
                            hit |= c >= set[k] && c <= set[k + 2];
                            k += 3;
                        } else {
                            hit |= c == set[k];
                            k += 1;
                        }
                    }
                    (hit != neg).then_some(end + 1 - i)
                }
                None => (c == b'[').then_some(1),
            },
            x => (x == c).then_some(1),
        }
    };
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < s.len() {
        if i < p.len() && p[i] == b'*' {
            star = Some((i, j));
            i += 1;
            continue;
        }
        if i < p.len() {
            if let Some(n) = item(i, s[j]) {
                i += n;
                j += 1;
                continue;
            }
        }
        match star {
            // Let the last `*` take one more byte and try again from there.
            Some((si, sj)) => {
                i = si + 1;
                j = sj + 1;
                star = Some((si, sj + 1));
            }
            None => return false,
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}
