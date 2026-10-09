//! A small git-style argument parser: short and long options, `--opt=value`, `-n5`, grouped
//! short flags, and `--` before paths.

use anyhow::{bail, Result};
use std::collections::HashMap;

#[derive(Default)]
pub struct Spec {
    /// Option names that take no value, for example `-s`, `--short`.
    flags: Vec<&'static str>,
    /// Option names that take a value.
    values: Vec<&'static str>,
    /// Long options with an optional value (`--porcelain`, `--porcelain=v1`).
    optional: Vec<&'static str>,
    /// Aliases: alias to canonical name.
    alias: HashMap<&'static str, &'static str>,
}

impl Spec {
    pub fn new() -> Spec {
        Spec::default()
    }
    /// A flag with names separated by `|`, for example `"-s|--short"`. The first is canonical.
    pub fn flag(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.flags.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
    pub fn value(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.values.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
    pub fn optional(mut self, names: &'static str) -> Spec {
        let v: Vec<&'static str> = names.split('|').collect();
        for n in &v {
            self.optional.push(n);
            self.alias.insert(n, v[0]);
        }
        self
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub flags: HashMap<String, usize>,
    pub values: HashMap<String, Vec<String>>,
    pub pos: Vec<String>,
    /// Arguments after `--`.
    pub paths: Vec<String>,
    pub dashdash: bool,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name) || self.values.contains_key(name)
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|v| v.last()).map(|s| s.as_str())
    }
    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }
    /// Positional arguments and paths after `--` together.
    pub fn rest(&self) -> Vec<String> {
        let mut v = self.pos.clone();
        v.extend(self.paths.iter().cloned());
        v
    }
}

pub fn parse(spec: &Spec, args: &[String]) -> Result<Parsed> {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if p.dashdash {
            p.paths.push(a.clone());
            continue;
        }
        if a == "--" {
            p.dashdash = true;
            continue;
        }
        if a.starts_with("--") {
            let (name, val) = match a.find('=') {
                Some(k) => (&a[..k], Some(a[k + 1..].to_string())),
                None => (a.as_str(), None),
            };
            if let Some(canon) = spec.alias.get(name) {
                if spec.flags.contains(&name) {
                    if val.is_some() {
                        bail!("option {name} takes no value");
                    }
                    *p.flags.entry(canon.to_string()).or_insert(0) += 1;
                } else if spec.values.contains(&name) {
                    let v = match val {
                        Some(v) => v,
                        None => {
                            if i >= args.len() {
                                bail!("option {name} needs a value");
                            }
                            i += 1;
                            args[i - 1].clone()
                        }
                    };
                    p.values.entry(canon.to_string()).or_default().push(v);
                } else {
                    p.values.entry(canon.to_string()).or_default().push(val.unwrap_or_default());
                }
                continue;
            }
            bail!("unknown option: {name}");
        }
        if a.starts_with('-') && a.len() > 1 {
            // `-<digits>`: a count, as in `log -5`.
            if a[1..].chars().all(|c| c.is_ascii_digit()) && spec.alias.contains_key("-n") {
                p.values.entry("-n".into()).or_default().push(a[1..].to_string());
                continue;
            }
            let chars: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let name = format!("-{}", chars[k]);
                let key: &str = name.as_str();
                let canon = match spec.alias.iter().find(|(n, _)| **n == key) {
                    Some((_, c)) => *c,
                    None => bail!("unknown option: {key}"),
                };
                if spec.optional.contains(&key) {
                    let rest: String = chars[k + 1..].iter().collect();
                    p.values.entry(canon.to_string()).or_default().push(rest);
                    break;
                }
                if spec.values.contains(&key) {
                    let rest: String = chars[k + 1..].iter().collect();
                    let v = if !rest.is_empty() {
                        rest
                    } else {
                        if i >= args.len() {
                            bail!("option {key} needs a value");
                        }
                        i += 1;
                        args[i - 1].clone()
                    };
                    p.values.entry(canon.to_string()).or_default().push(v);
                    break;
                }
                *p.flags.entry(canon.to_string()).or_insert(0) += 1;
                k += 1;
            }
            continue;
        }
        p.pos.push(a.clone());
    }
    Ok(p)
}
