//! Revision names: `HEAD`, `@`, line names, `line@machine`, tags, remote refs (`origin/main`),
//! state ids and their prefixes, git commit ids, `stash@{n}`, `HEAD@{n}` (automatic states,
//! newest first) and the suffixes `~n`, `^` and `^n`.

use crate::model::{Line, StateKind};
use crate::repo::Repo;
use anyhow::{bail, Result};

pub fn resolve(repo: &Repo, line: Option<&Line>, rev: &str) -> Result<String> {
    let (base, suffix) = split_suffix(rev);
    let (name, at) = match base.find("@{") {
        Some(i) if base.ends_with('}') => (&base[..i], Some(&base[i + 2..base.len() - 1])),
        _ => (base, None),
    };
    let mut id = if let Some(n) = at {
        let n: usize = n.parse().map_err(|_| anyhow::anyhow!("bad revision {rev}"))?;
        if name == "stash" {
            let l = need_line(line, rev)?;
            let v = repo.p.db.line_states(&l.id, StateKind::Stash)?;
            match v.get(n) {
                Some(s) => s.id.clone(),
                None => bail!("stash@{{{n}}} does not exist"),
            }
        } else {
            let l =
                if name.is_empty() || name == "HEAD" || name == "@" { need_line(line, rev)?.clone() } else { repo.line(name)? };
            let v = repo.p.db.line_states(&l.id, StateKind::Auto)?;
            match v.get(n) {
                Some(s) => s.id.clone(),
                None => bail!("{rev}: line '{}' has only {} automatic states", l.name, v.len()),
            }
        }
    } else {
        base_state(repo, line, name)?
    };
    let mut rest = suffix;
    while !rest.is_empty() {
        let c = rest.as_bytes()[0];
        let digits: String = rest[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
        rest = &rest[1 + digits.len()..];
        match c {
            b'~' => {
                let n: usize = if digits.is_empty() { 1 } else { digits.parse()? };
                for _ in 0..n {
                    id = match repo.parents(&id)?.first() {
                        Some(p) => p.clone(),
                        None => bail!("{rev}: state has no parent"),
                    };
                }
            }
            b'^' => {
                let n: usize = if digits.is_empty() { 1 } else { digits.parse()? };
                if n == 0 {
                    continue;
                }
                id = match repo.parents(&id)?.get(n - 1) {
                    Some(p) => p.clone(),
                    None => bail!("{rev}: state has no parent {n}"),
                };
            }
            _ => bail!("bad revision {rev}"),
        }
    }
    Ok(id)
}

fn need_line<'a>(line: Option<&'a Line>, rev: &str) -> Result<&'a Line> {
    match line {
        Some(l) => Ok(l),
        None => bail!("{rev}: not in a line"),
    }
}

fn split_suffix(rev: &str) -> (&str, &str) {
    // The suffix starts at the first '~' or '^' that is not inside "@{...}".
    let b = rev.as_bytes();
    let mut depth = 0;
    for (i, c) in b.iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'~' | b'^' if depth == 0 => return (&rev[..i], &rev[i..]),
            _ => {}
        }
    }
    (rev, "")
}

fn base_state(repo: &Repo, line: Option<&Line>, name: &str) -> Result<String> {
    if name.is_empty() || name == "HEAD" || name == "@" {
        return Ok(need_line(line, "HEAD")?.head.clone());
    }
    if name == "stash" {
        let l = need_line(line, name)?;
        return match repo.p.db.line_states(&l.id, StateKind::Stash)?.first() {
            Some(s) => Ok(s.id.clone()),
            None => bail!("no stash entries"),
        };
    }
    if let Some(l) = repo.p.db.line_by_name(name)? {
        return Ok(repo.localize(l)?.head);
    }
    if let Some((ln, m)) = name.split_once('@') {
        if let (Some(l), Some(mid)) = (repo.p.db.line_by_name(ln)?, repo.p.db.machine_by_name(m)?) {
            for (machine, head) in repo.p.db.line_heads(&l.id)? {
                if machine == mid {
                    return Ok(head);
                }
            }
            bail!("line '{ln}' has no head on machine {m}");
        }
    }
    if let Some(t) = repo.p.db.tag(name)? {
        return Ok(t);
    }
    if let Some((s, _)) = repo.p.db.remote_ref(name)? {
        return Ok(s);
    }
    if let Some((s, _)) = repo.p.db.remote_ref(&format!("origin/{name}"))? {
        if name.contains('/') {
            return Ok(s);
        }
    }
    if name.len() == 36 && uuid::Uuid::parse_str(name).is_ok() && repo.p.db.state(name)?.is_some() {
        return Ok(name.to_string());
    }
    if name.len() >= 4 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        let lower = name.to_ascii_lowercase();
        let v = repo.p.db.states_by_prefix(&lower)?;
        if v.len() == 1 {
            return Ok(v[0].clone());
        }
        if v.len() > 1 {
            bail!("short state id {name} is ambiguous");
        }
        let g = repo.p.db.state_of_commit(&lower)?;
        if g.len() == 1 {
            return Ok(g[0].clone());
        }
    }
    bail!("unknown revision '{name}'")
}
