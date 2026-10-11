//! The reader against the fixture vault that the Python writer made (js/vault/test/fixture): four
//! states with full and delta trees, delta chains, a link, an executable and an empty file.

use desk_core::{Reader, Store};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/vault/test/fixture");

struct Folder(PathBuf);

impl Store for Folder {
    async fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.0.join(name))?)
    }
}

fn reader() -> Reader<Folder> {
    let states = PathBuf::from(FIXTURE).join("objects/proj/states");
    let names: Vec<String> = std::fs::read_dir(&states)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let identity = std::fs::read_to_string(PathBuf::from(FIXTURE).join("identity.txt")).unwrap();
    Reader::new(Folder(states), &identity, &names).unwrap()
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
    let states = PathBuf::from(FIXTURE).join("objects/proj/states");
    let names: Vec<String> = std::fs::read_dir(&states)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let reader = Reader::new(
        Folder(states),
        &desk_core::keys::generate_identity(),
        &names,
    )
    .unwrap();
    assert!(reader.files(None).await.is_err());
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
