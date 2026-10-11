//! The vault reader (docs/design.md, "Reader"): the states of one vault, their trees and the
//! content of their files. It decrypts with the vault identity, checks each tree against the
//! vault's signature policy (docs/design.md, "Signatures") and each file against the size and
//! SHA-256 that its tree names.

use crate::format::{self, Entry, Header, Tree};
use crate::sign::Policy;
use crate::{keys, zst};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// Where the encrypted objects of one vault come from (S3, a gateway, a folder). Names are
/// relative to `<prefix>/states/`.
pub trait Store: Send + Sync {
    fn get(&self, name: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

/// A decoded tree: its header and canonical bytes.
type DecodedTree = (Header, Arc<Vec<u8>>);

/// Decrypted data parts kept in memory, at most this many bytes; trees are kept apart.
const PART_CACHE: usize = 512 << 20;
/// Decoded trees kept in memory, at most this many bytes.
const TREE_CACHE: usize = 256 << 20;

#[derive(Default)]
struct Cache {
    parts: HashMap<String, Arc<Vec<u8>>>,
    bytes: usize,
    /// Trees whose signature, when they need one, is checked.
    trees: HashMap<(u64, String), DecodedTree>,
    tree_bytes: usize,
}

pub struct Reader<S> {
    store: S,
    identity: age::x25519::Identity,
    policy: Policy,
    trees: BTreeMap<u64, Vec<String>>,
    cache: Mutex<Cache>,
}

impl<S: Store> Reader<S> {
    /// `names` are the object names below `states/` (a listing); `identity` is the vault
    /// identity, which must belong to the recipient of `policy`.
    pub fn new(store: S, identity: &str, names: &[String], policy: Policy) -> Result<Self> {
        policy.check_identity(identity)?;
        let identity = age::x25519::Identity::from_str(identity.trim())
            .map_err(|e| anyhow!("not an age identity: {e}"))?;
        let mut trees: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for name in names {
            if let Some((seq, attempt)) = format::parse_tree_name(name) {
                trees.entry(seq).or_default().push(attempt);
            }
        }
        for attempts in trees.values_mut() {
            attempts.sort();
        }
        Ok(Reader {
            store,
            identity,
            policy,
            trees,
            cache: Mutex::default(),
        })
    }

    /// Whether state `seq` carries a checked signature: reading it fails without a valid one.
    /// The other states read as before and show as unsigned.
    pub fn signed(&self, seq: u64) -> bool {
        self.policy.signed(seq)
    }

    /// Every pushed state, oldest first. A second tree of one `seq` (after a lost reply) is
    /// harmless; the first attempt in sort order stands for it.
    pub fn states(&self) -> Vec<(u64, String)> {
        self.trees
            .iter()
            .map(|(seq, a)| (*seq, a[0].clone()))
            .collect()
    }

    pub fn latest(&self) -> Result<(u64, String)> {
        self.states()
            .pop()
            .ok_or_else(|| anyhow!("the vault has no state yet"))
    }

    async fn decrypted(&self, name: &str) -> Result<Vec<u8>> {
        let data = self
            .store
            .get(name)
            .await
            .with_context(|| format!("reading {name}"))?;
        keys::decrypt_with(&self.identity, &data).with_context(|| format!("decrypting {name}"))
    }

    async fn part(&self, name: &str) -> Result<Arc<Vec<u8>>> {
        if let Some(part) = self.cache.lock().unwrap().parts.get(name) {
            return Ok(part.clone());
        }
        let part = Arc::new(self.decrypted(name).await?);
        let mut cache = self.cache.lock().unwrap();
        if cache.bytes + part.len() > PART_CACHE {
            cache.parts.clear();
            cache.bytes = 0;
        }
        cache.bytes += part.len();
        cache.parts.insert(name.to_string(), part.clone());
        Ok(part)
    }

    /// The header of one state. The tree of a state that must be signed is decoded to check its
    /// signature; the header of another state comes without it.
    pub async fn header(&self, seq: u64, attempt: &str) -> Result<Header> {
        if self.policy.signed(seq) {
            return Ok(self.tree(seq, attempt).await?.0);
        }
        if let Some((header, _)) = self
            .cache
            .lock()
            .unwrap()
            .trees
            .get(&(seq, attempt.to_string()))
        {
            return Ok(header.clone());
        }
        let object = self.decrypted(&format::name(seq, attempt, None)).await?;
        Ok(format::split_tree(&object)?.0)
    }

    /// The header and the canonical tree bytes of one state. Each tree on the way is checked
    /// against the signature policy before it is kept.
    pub async fn tree(&self, seq: u64, attempt: &str) -> Result<(Header, Arc<Vec<u8>>)> {
        // Walk back to the nearest full tree (or one already decoded), then decode forward.
        let mut pending = Vec::new();
        let mut at = (seq, attempt.to_string());
        let mut canonical = loop {
            if let Some((header, bytes)) = self.cache.lock().unwrap().trees.get(&at) {
                if pending.is_empty() {
                    return Ok((header.clone(), bytes.clone()));
                }
                break bytes.clone();
            }
            let object = self.decrypted(&format::name(at.0, &at.1, None)).await?;
            let (header, frame) = format::split_tree(&object)?;
            if (header.seq, header.attempt.as_str()) != (at.0, at.1.as_str()) {
                bail!(
                    "tree {}.{} names itself {}.{}",
                    at.0,
                    at.1,
                    header.seq,
                    header.attempt
                );
            }
            if header.tree == "full" {
                let bytes = Arc::new(zst::decompress(frame, None)?);
                self.policy.check(&header, &bytes)?;
                self.remember(&header, &bytes);
                if pending.is_empty() {
                    return Ok((header, bytes));
                }
                break bytes;
            }
            if pending.len() > self.trees.len() {
                bail!("the tree chain of {seq}.{attempt} has a loop");
            }
            let base = header.base.clone().expect("checked in split_tree");
            pending.push((header, frame.to_vec()));
            at = base;
        };
        let mut last = None;
        while let Some((header, frame)) = pending.pop() {
            canonical = Arc::new(zst::decompress(&frame, Some(&canonical))?);
            self.policy.check(&header, &canonical)?;
            self.remember(&header, &canonical);
            last = Some(header);
        }
        Ok((last.expect("one delta at least"), canonical))
    }

    fn remember(&self, header: &Header, bytes: &Arc<Vec<u8>>) {
        let key = (header.seq, header.attempt.clone());
        let mut cache = self.cache.lock().unwrap();
        if cache.tree_bytes + bytes.len() > TREE_CACHE {
            cache.trees.clear();
            cache.tree_bytes = 0;
        }
        cache.tree_bytes += bytes.len();
        cache.trees.insert(key, (header.clone(), bytes.clone()));
    }

    /// `path -> entry` of one state (the latest when `state` is none).
    pub async fn files(&self, state: Option<(u64, String)>) -> Result<BTreeMap<String, Entry>> {
        let (seq, attempt) = match state {
            Some(s) => s,
            None => self.latest()?,
        };
        let (_, canonical) = self.tree(seq, &attempt).await?;
        let tree: Tree = serde_json::from_slice(&canonical).context("the tree is not valid")?;
        for path in tree.files.keys() {
            if !safe_path(path) {
                bail!("the tree names an unsafe path {path:?}");
            }
        }
        Ok(tree.files)
    }

    /// The content of one tree entry: its chain decoded from the full frame up, checked against
    /// the entry's size and SHA-256.
    pub async fn read(&self, entry: &Entry) -> Result<Vec<u8>> {
        let chain = &entry.3;
        if chain.is_empty() || chain.last().is_some_and(|l| l.is_delta()) {
            bail!("a file chain must end with a full frame");
        }
        let mut data: Option<Vec<u8>> = None;
        for link in chain.iter().rev() {
            let part = self.part(&link.object()).await?;
            let frame = part
                .get(link.range())
                .ok_or_else(|| anyhow!("a frame is outside its part"))?;
            data = Some(if link.is_delta() {
                zst::decompress(frame, data.as_deref())?
            } else {
                zst::decompress(frame, None)?
            });
        }
        let data = data.unwrap_or_default();
        if data.len() as u64 != entry.size() || hex::encode(Sha256::digest(&data)) != entry.sha256()
        {
            bail!("the content does not match its tree entry");
        }
        Ok(data)
    }
}

/// A relative path without `.`, `..` or empty parts, as the writer makes them.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
