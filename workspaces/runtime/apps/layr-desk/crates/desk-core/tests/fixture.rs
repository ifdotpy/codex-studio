//! The reader against the fixture vault that the Python writer made (js/vault/test/fixture): four
//! states with full and delta trees, delta chains, a link, an executable and an empty file. Then
//! the same states signed with the test key of fixture/signed.json, whose signatures Node made:
//! signed states read, and changed, unsigned or wrongly signed states and slots are refused.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use desk_core::sign::Signer;
use desk_core::{keys, Policy, Reader, Store, VaultInfo};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/vault/test/fixture");

struct Folder(PathBuf);

impl Store for Folder {
    async fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.0.join(name))?)
    }
}

fn states_dir() -> PathBuf {
    PathBuf::from(FIXTURE).join("objects/proj/states")
}

fn names() -> Vec<String> {
    std::fs::read_dir(states_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn identity() -> String {
    std::fs::read_to_string(PathBuf::from(FIXTURE).join("identity.txt"))
        .unwrap()
        .trim()
        .to_string()
}

fn json(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(PathBuf::from(FIXTURE).join(name)).unwrap())
        .unwrap()
}

/// The fixture vault as vault.json describes it, with a writer key and `signedFrom` or without.
fn policy(writer: Option<&str>, signed_from: Option<u64>) -> Policy {
    let vault = VaultInfo {
        recipient: keys::recipient_of(&identity()).unwrap(),
        writer: writer.map(str::to_string),
        signed_from,
    };
    Policy::new("proj", &vault).unwrap()
}

fn reader() -> Reader<Folder> {
    Reader::new(
        Folder(states_dir()),
        &identity(),
        &names(),
        policy(None, None),
    )
    .unwrap()
}

/// Objects in memory, by name below states/.
#[derive(Clone)]
struct Memory(HashMap<String, Vec<u8>>);

impl Store for Memory {
    async fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        self.0
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no object {name}"))
    }
}

impl Memory {
    fn fixture() -> Memory {
        Memory(
            names()
                .into_iter()
                .map(|n| (n.clone(), std::fs::read(states_dir().join(&n)).unwrap()))
                .collect(),
        )
    }

    fn tree_name(&self, seq: u64) -> String {
        let start = format!("{seq:010}.");
        self.0
            .keys()
            .find(|n| n.starts_with(&start) && n.ends_with(".tree.age"))
            .unwrap()
            .clone()
    }

    /// Decrypt the tree of `seq`, change its header and its frame, and encrypt it again to the
    /// vault recipient, as anyone who can write to the bucket can.
    fn change(&mut self, seq: u64, change: impl FnOnce(&mut Value, &mut Vec<u8>)) {
        let name = self.tree_name(seq);
        let plain = keys::decrypt(&identity(), &self.0[&name]).unwrap();
        let newline = plain.iter().position(|&b| b == b'\n').unwrap();
        let mut header: Value = serde_json::from_slice(&plain[..newline]).unwrap();
        let mut frame = plain[newline + 1..].to_vec();
        change(&mut header, &mut frame);
        let mut object = serde_json::to_vec(&header).unwrap();
        object.push(b'\n');
        object.extend(frame);
        let recipient = keys::recipient_of(&identity()).unwrap();
        self.0
            .insert(name, keys::encrypt(&recipient, &object).unwrap());
    }

    fn reader(&self, policy: Policy) -> Reader<Memory> {
        let names: Vec<String> = self.0.keys().cloned().collect();
        Reader::new(self.clone(), &identity(), &names, policy).unwrap()
    }
}

/// The fixture with the signatures of signed.json on the states from its `signedFrom` on.
fn signed() -> (Memory, Policy) {
    let vectors = json("signed.json");
    let signed_from = vectors["signedFrom"].as_u64().unwrap();
    let mut store = Memory::fixture();
    for state in vectors["states"].as_array().unwrap() {
        let seq = state["seq"].as_u64().unwrap();
        if seq >= signed_from {
            let sig = state["sig"].clone();
            store.change(seq, |header, _| header["sig"] = sig);
        }
    }
    let writer = vectors["writer"].as_str().unwrap();
    (store, policy(Some(writer), Some(signed_from)))
}

async fn error_of<S: Store>(reader: &Reader<S>, seq: u64) -> String {
    let state = reader.states().into_iter().find(|s| s.0 == seq).unwrap();
    reader.files(Some(state)).await.unwrap_err().to_string()
}

#[tokio::test]
async fn every_state_reads_as_the_writer_wrote_it() {
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(FIXTURE).join("expected.json")).unwrap(),
    )
    .unwrap();
    let reader = reader();
    let states = reader.states();
    let want = expected["states"].as_array().unwrap();
    assert_eq!(states.len(), want.len());
    for (state, want) in states.into_iter().zip(want) {
        let files = reader.files(Some(state.clone())).await.unwrap();
        let want = want.as_object().unwrap();
        assert_eq!(files.len(), want.len(), "state {state:?}");
        for (path, entry) in &files {
            let data = reader.read(entry).await.unwrap();
            assert_eq!(
                hex::encode(Sha256::digest(&data)),
                want[path][0],
                "{path} in {state:?}"
            );
            assert_eq!(entry.mode(), want[path][1], "{path} in {state:?}");
        }
    }
}

#[tokio::test]
async fn the_wrong_identity_reads_nothing() {
    let stranger = keys::generate_identity();
    // The vault's own reader refuses it at once: a forged slot holds such a key.
    let err = Reader::new(
        Folder(states_dir()),
        &stranger,
        &names(),
        policy(None, None),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(err.contains("may be forged"), "{err}");
    // With a vault.json that names its recipient, it still decrypts nothing.
    let lying = Policy::new(
        "proj",
        &VaultInfo {
            recipient: keys::recipient_of(&stranger).unwrap(),
            ..VaultInfo::default()
        },
    )
    .unwrap();
    let reader = Reader::new(Folder(states_dir()), &stranger, &names(), lying).unwrap();
    assert!(reader.files(None).await.is_err());
}

#[tokio::test]
async fn signatures_match_the_ones_that_node_made() {
    let vectors = json("signed.json");
    let signer = Signer::from_base64(vectors["seed"].as_str().unwrap()).unwrap();
    assert_eq!(signer.public(), vectors["writer"]);
    let reader = reader();
    for (state, want) in reader
        .states()
        .into_iter()
        .zip(vectors["states"].as_array().unwrap())
    {
        assert_eq!(want["seq"], state.0);
        let (header, canonical) = reader.tree(state.0, &state.1).await.unwrap();
        let message = desk_core::sign::message("proj", &header, &canonical);
        assert_eq!(message, want["message"], "state {}", state.0);
        assert_eq!(
            signer.sign("proj", &header, &canonical),
            want["sig"],
            "state {}",
            state.0
        );
    }
}

#[tokio::test]
async fn a_signed_vault_reads_and_marks_older_states_unsigned() {
    let expected = json("expected.json");
    let (store, policy) = signed();
    let reader = store.reader(policy);
    let marks: Vec<bool> = reader.states().iter().map(|s| reader.signed(s.0)).collect();
    assert_eq!(marks, [false, true, true, true]);
    for (state, want) in reader
        .states()
        .into_iter()
        .zip(expected["states"].as_array().unwrap())
    {
        let header = reader.header(state.0, &state.1).await.unwrap();
        assert_eq!(header.sig.is_some(), state.0 >= 2);
        let files = reader.files(Some(state)).await.unwrap();
        for (path, entry) in &files {
            let data = reader.read(entry).await.unwrap();
            assert_eq!(hex::encode(Sha256::digest(&data)), want[path][0]);
        }
    }
}

#[tokio::test]
async fn a_changed_unsigned_or_wrongly_signed_state_is_refused() {
    let vectors = json("signed.json");
    let sig_of = |seq: usize| vectors["states"][seq - 1]["sig"].clone();
    let (signed, policy) = signed();

    // No signature, the signature of another state, and one that is not 64 bytes.
    let mut store = signed.clone();
    store.change(4, |header, _| {
        header.as_object_mut().unwrap().remove("sig");
    });
    let reader = store.reader(policy.clone());
    let err = error_of(&reader, 4).await;
    assert!(err.contains("state 4 has no writer signature"), "{err}");
    let latest = reader.latest().unwrap();
    assert!(reader.header(latest.0, &latest.1).await.is_err());
    assert!(reader.files(Some((3, attempt(&vectors, 3)))).await.is_ok());

    let mut store = signed.clone();
    store.change(4, |header, _| header["sig"] = sig_of(3));
    let err = error_of(&store.reader(policy.clone()), 4).await;
    assert!(err.contains("does not match"), "{err}");

    let mut store = signed.clone();
    store.change(4, |header, _| header["sig"] = B64.encode([0u8; 32]).into());
    let err = error_of(&store.reader(policy.clone()), 4).await;
    assert!(err.contains("64 bytes"), "{err}");

    // A changed header field, or a changed tree, under the right signature.
    let mut store = signed.clone();
    store.change(4, |header, _| header["time"] = 1.into());
    let err = error_of(&store.reader(policy.clone()), 4).await;
    assert!(err.contains("does not match"), "{err}");

    let reader = signed.reader(policy.clone());
    let (_, base) = reader.tree(3, &attempt(&vectors, 3)).await.unwrap();
    let (_, tree) = reader.tree(4, &attempt(&vectors, 4)).await.unwrap();
    let mut changed: Value = serde_json::from_slice(&tree).unwrap();
    changed["files"]["src/app.py"][1] = 1.into();
    let forged = serde_json::to_vec(&changed).unwrap();
    let mut store = signed.clone();
    store.change(4, |_, frame| {
        *frame = desk_core::zst::compress(&forged, Some(&base)).unwrap();
    });
    let err = error_of(&store.reader(policy.clone()), 4).await;
    assert!(err.contains("does not match"), "{err}");

    // A changed base tree breaks the trees after it as well.
    let mut store = signed.clone();
    store.change(2, |header, _| header["sig"] = sig_of(3));
    let reader = store.reader(policy);
    for seq in [2, 3, 4] {
        assert!(error_of(&reader, seq).await.contains("state 2"), "{seq}");
    }
    assert!(reader.files(Some((1, attempt(&vectors, 1)))).await.is_ok());
}

fn attempt(vectors: &Value, seq: usize) -> String {
    vectors["states"][seq - 1]["attempt"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn states_below_signed_from_read_as_before() {
    let writer = json("signed.json")["writer"].as_str().unwrap().to_string();
    let store = Memory::fixture();
    let reader = store.reader(policy(Some(&writer), Some(5)));
    for state in reader.states() {
        assert!(!reader.signed(state.0));
        reader.files(Some(state)).await.unwrap();
    }
    let reader = store.reader(policy(Some(&writer), Some(4)));
    assert!(error_of(&reader, 4).await.contains("no writer signature"));
    reader
        .files(Some((3, attempt(&json("signed.json"), 3))))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_changed_entry_is_refused() {
    let reader = reader();
    let files = reader.files(None).await.unwrap();
    let (_, entry) = files.iter().find(|(_, e)| e.size() > 0).unwrap();
    let mut wrong = entry.clone();
    wrong.2 = "0".repeat(64);
    assert!(reader
        .read(&wrong)
        .await
        .unwrap_err()
        .to_string()
        .contains("does not match"));
}

#[test]
fn unsafe_paths_are_refused() {
    for bad in ["", "/etc/passwd", "a/../b", "./a", "a//b", "a/."] {
        assert!(!desk_core::reader::safe_path(bad), "{bad}");
    }
    assert!(desk_core::reader::safe_path("src/app.py"));
}
