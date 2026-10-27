//! Exact-value checks of shared changes. The private index holds HMACs only.

use crate::ctx::Ctx;
use crate::fsutil;
use crate::repo::{Repo, Snap};
use anyhow::{bail, Context, Result};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use std::collections::{BTreeSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

type HmacSha256 = Hmac<Sha256>;

#[derive(Default)]
struct Index {
    hashes: BTreeSet<String>,
    max_len: usize,
}
impl Index {
    fn add(&mut self, key: &[u8], value: &str) {
        if candidate(value) {
            self.hashes.insert(digest(key, value.as_bytes()));
            self.max_len = self.max_len.max(value.len());
        }
    }
}

fn digest(key: &[u8], value: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(value);
    hex::encode(mac.finalize().into_bytes())
}

fn candidate(value: &str) -> bool {
    value.chars().count() >= 12
        && !(value.len() < 20 && (value.bytes().all(|b| b.is_ascii_alphabetic()) || value.bytes().all(|b| b.is_ascii_digit())))
}

fn add_values(key: &[u8], data: &[u8], index: &mut Index) {
    add_source(key, data, index, true);
}

fn add_source(key: &[u8], data: &[u8], index: &mut Index, whole: bool) {
    let mut add = |v: &str| index.add(key, v);
    if whole && data.len() <= 4096 {
        if let Ok(text) = std::str::from_utf8(data) {
            add(text);
            add(text.trim_end_matches(['\r', '\n']));
        }
    }
    for line in data.split(|b| *b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else { continue };
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim().strip_prefix("export ").unwrap_or(name.trim());
            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                let value = value.trim();
                let value = if value.len() >= 2
                    && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
                {
                    &value[1..value.len() - 1]
                } else {
                    value
                };
                add(value);
            }
        }
    }
}

fn project_key(repo: &Repo) -> Result<Vec<u8>> {
    let path = repo.p.meta_dir().join("secrets.key");
    if !path.exists() {
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        let temporary = repo.p.meta_dir().join(format!(".secrets-key-{}", crate::ids::new_id()));
        let mut f =
            OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&temporary)?;
        f.write_all(&key)?;
        f.sync_all()?;
        let result = std::fs::hard_link(&temporary, &path);
        let _ = std::fs::remove_file(&temporary);
        match result {
            Ok(()) => std::fs::File::open(repo.p.meta_dir())?.sync_all()?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut key = Vec::new();
    OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path)?.take(33).read_to_end(&mut key)?;
    if key.len() != 32 {
        bail!("invalid project secret index key");
    }
    Ok(key)
}

fn source_file(root: &Path, path: &str, key: &[u8], index: &mut Index) -> Result<()> {
    if !fsutil::lmeta(root, path).is_some_and(|m| m.is_file()) {
        return Ok(());
    }
    let f = crate::tree::Tree::open(root)?.open_read(path)?;
    if f.metadata()?.len() <= 4096 {
        let mut data = Vec::new();
        f.take(4097).read_to_end(&mut data)?;
        add_values(key, &data, index);
    } else {
        let mut reader = std::io::BufReader::new(f);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line)? != 0 {
            add_source(key, &line, index, false);
            line.clear();
        }
    }
    Ok(())
}

/// Rebuild on each check, including uncommitted source changes. No source bytes reach disk.
fn index(ctx: &Ctx, repo: &Repo, key: &[u8]) -> Result<Index> {
    let mut hashes = Index::default();
    let mut overlay_paths = BTreeSet::new();
    for name in repo.overlay_names()? {
        // A machine without the overlay key has no overlay project and no values to protect.
        let Ok(overlay) = repo.overlay_repo(&name) else { continue };
        if !repo.reads_overlay(&name, ctx.caller.uid, ctx.caller.gid)? {
            continue;
        }
        for line in overlay.lines()? {
            let root = overlay.state_root(&line.head)?;
            for path in crate::overlay::files_of(&root)? {
                source_file(&root, &path, key, &mut hashes)?;
                overlay_paths.insert(path);
            }
        }
    }
    let configured = repo.p.config_value("secrets.paths")?;
    let patterns: Vec<String> = match configured {
        None => Vec::new(),
        Some(serde_json::Value::String(v)) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(serde_json::Value::Array(v)) => v
            .into_iter()
            .map(|p| p.as_str().map(str::to_string).context("secrets.paths must contain strings"))
            .collect::<Result<_>>()?,
        Some(_) => bail!("secrets.paths must be a string or an array of patterns"),
    };
    for line in repo.lines()? {
        // A root caller may inspect all project lines; other callers use their own files.
        if line.owner != ctx.caller.uid && !ctx.caller.is_root() {
            continue;
        }
        overlay_paths.extend(repo.pending_moves(&line).into_keys());
        let root = repo.working(&line);
        if !root.exists() {
            continue;
        }
        let mut matcher = ignore::gitignore::GitignoreBuilder::new(&root);
        for pattern in &patterns {
            matcher.add_line(None, pattern)?;
        }
        let matcher = matcher.build()?;
        for path in crate::overlay::files_of(&root)? {
            if overlay_paths.contains(&path) || matcher.matched_path_or_any_parents(&path, false).is_ignore() {
                source_file(&root, &path, key, &mut hashes)?;
            }
        }
    }
    fsutil::atomic_write(
        &repo.p.meta_dir().join(format!("secrets-index-{}.json", ctx.caller.uid)),
        &serde_json::to_vec(&hashes.hashes)?,
        0o600,
    )?;
    Ok(hashes)
}

fn allowed(repo: &Repo) -> Result<BTreeSet<(String, String)>> {
    Ok(repo
        .p
        .db
        .declassifications()?
        .into_iter()
        .filter_map(|r| {
            Some((r.data["declassify"]["path"].as_str()?.to_string(), r.data["declassify"]["hmac"].as_str()?.to_string()))
        })
        .collect())
}

fn matches_line(key: &[u8], index: &Index, line: &str) -> BTreeSet<String> {
    static TOKENS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tokens = TOKENS.get_or_init(|| regex::Regex::new(r"[A-Za-z0-9_+/=.-]{12,}").expect("constant expression"));
    let mut found = BTreeSet::new();
    for token in tokens.find_iter(line) {
        let mut value = token.as_str();
        if value.len() > index.max_len + 1 {
            value = &value[value.len() - index.max_len - 1..];
        }
        loop {
            let hash = digest(key, value.as_bytes());
            if index.hashes.contains(&hash) {
                found.insert(hash);
            }
            // KEY=value uses the same alphabet as a value, including base64 padding.
            match value.split_once('=') {
                Some((_, suffix)) if suffix.len() >= 12 => value = suffix,
                _ => break,
            }
        }
    }
    found
}

fn refusal(path: &str, location: &str, hash: &str, rev: &str) -> anyhow::Error {
    let scope = format!("{path}@{}", &hash[..16]);
    let quoted = format!("'{}'", scope.replace('\'', "'\"'\"'"));
    let display = crate::ids::display(rev);
    let rev = if display != rev && display.len() > 7 { display[..7].to_string() } else { rev.to_string() };
    anyhow::anyhow!(
        "secret detected at {path}:{location}; declassify id {scope};\n\
         keep the value in an overlay, or an admin marks it as not secret: layr approve --declassify {quoted} {rev}"
    )
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_+/=.-".contains(&b)
}

/// Keep the token suffix across chunk boundaries, bounded by the longest indexed value.
/// One extra byte prevents a suffix of a longer token from matching a whole value.
fn scan_large(
    root: &(impl fsutil::Src + ?Sized),
    path: &str,
    key: &[u8],
    index: &Index,
    approvals: &BTreeSet<(String, String)>,
    rev: &str,
) -> Result<()> {
    let mut file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(root.at(path)?)?;
    let mut chunk = vec![0u8; 1 << 20];
    let mut token = VecDeque::new();
    let mut offset = 0u64;
    let mut start = 0u64;
    let mut length = 0u64;
    let finish = |token: &mut VecDeque<u8>, start: u64, length: u64| -> Result<()> {
        if length >= 12 {
            let text = std::str::from_utf8(token.make_contiguous())?;
            for hash in matches_line(key, index, text) {
                if !approvals.contains(&(path.to_string(), hash.clone())) {
                    return Err(refusal(path, &format!("byte {start}"), &hash, rev));
                }
            }
        }
        token.clear();
        Ok(())
    };
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        for &b in &chunk[..n] {
            if token_byte(b) {
                if length == 0 {
                    start = offset;
                }
                token.push_back(b);
                if token.len() > index.max_len + 1 {
                    token.pop_front();
                }
                length += 1;
            } else {
                finish(&mut token, start, length)?;
                length = 0;
            }
            offset += 1;
        }
    }
    finish(&mut token, start, length)?;
    Ok(())
}

fn check_tree(ctx: &Ctx, repo: &Repo, old: Option<&Snap>, new: &Snap, rev: &str) -> Result<()> {
    if repo.p.is_overlay() {
        return Ok(());
    }
    let key = project_key(repo)?;
    let hashes = index(ctx, repo, &key)?;
    if hashes.hashes.is_empty() {
        return Ok(());
    }
    let approvals = allowed(repo)?;
    let files = match old {
        Some(old) => crate::diff::file_diffs(old, new, &crate::changes::file_changes(old, new)?)?,
        None => crate::overlay::files_of(new)?
            .into_iter()
            .map(|path| {
                let meta = fsutil::lmeta(new, &path).context("secret check file disappeared")?;
                Ok(crate::diff::FileDiff { old: None, new: Some(crate::diff::side_of(new, &path, &meta)?) })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    for file in files {
        let Some(side) = &file.new else { continue };
        if side.big.is_some() {
            scan_large(new, &side.path, &key, &hashes, &approvals, rev)?;
            continue;
        }
        let old = file.old.as_ref().map(|s| String::from_utf8_lossy(&s.data)).unwrap_or_default();
        let new = String::from_utf8_lossy(&side.data);
        let diff = similar::TextDiff::from_lines(old.as_ref(), new.as_ref());
        for change in diff.iter_all_changes().filter(|c| c.tag() == similar::ChangeTag::Insert) {
            let line = change.new_index().unwrap_or(0) + 1;
            for hash in matches_line(&key, &hashes, change.value()) {
                if !approvals.contains(&(side.path.clone(), hash.clone())) {
                    return Err(refusal(&side.path, &line.to_string(), &hash, rev));
                }
            }
        }
    }
    Ok(())
}

pub fn check_between(ctx: &Ctx, repo: &Repo, old: &Snap, new: &Snap, rev: &str) -> Result<()> {
    check_tree(ctx, repo, Some(old), new, rev)
}

pub fn check_state(ctx: &Ctx, repo: &Repo, state: &str) -> Result<()> {
    if repo.p.is_overlay() {
        return Ok(());
    }
    let s = repo.state(state)?;
    let root = repo.state_src(state)?;
    match s.parents.first() {
        Some(parent) => check_between(ctx, repo, &repo.state_src(parent)?, &root, state),
        None => check_tree(ctx, repo, None, &root, state),
    }
}

/// A folder export sends only the selected tree, including unchanged files.
pub fn check_contents(ctx: &Ctx, repo: &Repo, state: &str) -> Result<()> {
    check_tree(ctx, repo, None, &repo.state_src(state)?, state)
}

/// Exports include history, so inspect every state that can leave the machine.
pub fn check_history(ctx: &Ctx, repo: &Repo, state: &str) -> Result<()> {
    let mut pending = vec![state.to_string()];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        check_state(ctx, repo, &id)?;
        pending.extend(repo.state(&id)?.parents);
    }
    Ok(())
}

pub fn declassify(ctx: &Ctx, repo: &Repo, scope: &str, state: &str) -> Result<crate::model::Record> {
    if !repo.is_admin(ctx) {
        bail!("declassification requires the admin role");
    }
    let (path, prefix) = scope.rsplit_once('@').context("use --declassify <path>@<id> from the refusal")?;
    fsutil::check_rel(path)?;
    if prefix.len() != 16 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("declassify id must contain 16 hexadecimal characters");
    }
    let key = project_key(repo)?;
    let hashes = index(ctx, repo, &key)?;
    let found: Vec<_> = hashes.hashes.iter().filter(|h| h.starts_with(prefix)).collect();
    if found.len() != 1 {
        bail!("declassify id is unknown or ambiguous; repeat the refused operation");
    }
    let payload = serde_json::json!({"declassify":{"path":path,"hmac":found[0]}});
    let mut rec = crate::cmd::attest::record(repo, state, "approval", &serde_json::to_vec(&payload)?)?;
    rec.data["declassify"] = payload["declassify"].clone();
    Ok(rec)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn candidates_and_exact_tokens() {
        let key = [7; 32];
        let mut index = Index::default();
        add_values(&key, b"A=sk-real_123456789\nB=abcdefghijklmnop\nC=123456789012345\nD=12345678901234567890\n", &mut index);
        assert!(index.hashes.contains(&digest(&key, b"sk-real_123456789")));
        assert!(!index.hashes.contains(&digest(&key, b"abcdefghijklmnop")));
        assert!(!index.hashes.contains(&digest(&key, b"123456789012345")));
        assert!(index.hashes.contains(&digest(&key, b"12345678901234567890")));
        add_values(&key, b"\xff\nTOKEN=sk_binary_1234567890\n", &mut index);
        assert!(index.hashes.contains(&digest(&key, b"sk_binary_1234567890")));
        assert!(candidate("abcdefghijklmnopqrst"));
        assert!(!candidate("abcdefghijklmnopqrs"));
        assert_eq!(matches_line(&key, &index, "password=sk-real_123456789").len(), 1);
        assert!(matches_line(&key, &index, "prefixsk-real_123456789suffix").is_empty());
        assert_eq!(matches_line(&key, &index, "'sk-real_123456789'").len(), 1);
    }
    #[test]
    fn whole_file_limit_and_quotes() {
        let key = [9; 32];
        let mut index = Index::default();
        add_values(&key, b"TOKEN='aBcd_1234567890'\n", &mut index);
        assert!(index.hashes.contains(&digest(&key, b"aBcd_1234567890")));
        add_values(&key, &vec![b'_'; 4097], &mut index);
        assert!(!index.hashes.contains(&digest(&key, &vec![b'_'; 4097])));
    }
}
