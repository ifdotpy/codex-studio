//! The writer against a fake layr (states as folders) and a fake bucket (objects as files): a
//! port of python/test_vault_writer.py, where the reader rebuilds every pushed state. The cross
//! tests run the Python writer and reader (python/, through tests/cross.py) on the same folders:
//! the Python reader reads what this writer pushed, and each writer continues a state folder of
//! the other.

use anyhow::{bail, Result};
use desk_core::writer::{self, Bucket, Layr, Summary, Writer, MAX_DEPTH};
use desk_core::{keys, zst, Reader, Store};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// `path -> (content, mode)`.
type Files = BTreeMap<String, (Vec<u8>, &'static str)>;

fn files(list: &[(&str, &[u8], &'static str)]) -> Files {
    list.iter()
        .map(|(p, d, m)| (p.to_string(), (d.to_vec(), *m)))
        .collect()
}

/// About 25 kB of source text, the same for every run.
fn source() -> Vec<u8> {
    let mut seed = 1u64;
    let mut word = || {
        let mut w = String::new();
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        for i in 0..4 + (seed >> 60) {
            w.push(b"abcdefghijklmnopqrstuvwxyz_"[((seed >> (i * 3)) % 27) as usize] as char);
        }
        w
    };
    let mut out = String::new();
    for i in 0..400 {
        out += &format!(
            "def {}({}, {}):\n    return {}({}) * {i}\n\n",
            word(),
            word(),
            word(),
            word(),
            word()
        );
    }
    out.into_bytes()
}

struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Temp {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "desk-writer-{name}-{}-{}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_files(folder: &Path, files: &Files) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(folder).unwrap();
    for (path, (data, mode)) in files {
        let target = folder.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if *mode == "120000" {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(data), &target).unwrap();
        } else {
            std::fs::write(&target, data).unwrap();
            let bits = if *mode == "100755" { 0o755 } else { 0o644 };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(bits)).unwrap();
        }
    }
}

fn snapshot(folder: &Path) -> Files {
    writer::walk(folder)
        .unwrap()
        .into_iter()
        .map(|(p, path)| (p, writer::content(&path).unwrap()))
        .collect()
}

/// layr with each state a folder `root/<state>`: `export` copies it, `diff` compares two and
/// names the paths as layr does, without quoting.
struct FakeLayr {
    root: PathBuf,
}

impl FakeLayr {
    fn commit(&self, state: &str, files: &Files) {
        write_files(&self.root.join(state), files);
    }

    fn state(&self, state: &str) -> Files {
        snapshot(&self.root.join(state))
    }
}

impl Layr for &FakeLayr {
    fn run(&self, args: &[&str]) -> Result<String> {
        match args {
            ["export", state, folder, "-q"] => {
                let folder = Path::new(folder);
                let _ = std::fs::remove_dir_all(folder);
                write_files(folder, &self.state(state));
                std::fs::write(
                    folder.join(writer::MARKER),
                    format!("{{\"state\": \"{state}\"}}"),
                )?;
                Ok(String::new())
            }
            ["diff", "--name-status", old, new] => {
                let (old, new) = (self.state(old), self.state(new));
                let mut lines: Vec<String> = Vec::new();
                for (path, file) in &new {
                    match old.get(path) {
                        None => lines.push(format!("A\t{path}")),
                        Some(before) if before != file => lines.push(format!("M\t{path}")),
                        _ => {}
                    }
                }
                lines.extend(
                    old.keys()
                        .filter(|p| !new.contains_key(*p))
                        .map(|p| format!("D\t{p}")),
                );
                lines.sort();
                Ok(lines.join("\n"))
            }
            _ => bail!("fake layr: {args:?}"),
        }
    }
}

/// A bucket whose objects are files below `root`; it can fail the puts of keys with a suffix.
struct FakeBucket {
    root: PathBuf,
    puts: Mutex<Vec<String>>,
    fail: Mutex<Option<&'static str>>,
}

impl Bucket for &FakeBucket {
    async fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        if let Some(suffix) = *self.fail.lock().unwrap() {
            if key.ends_with(suffix) {
                bail!("the reply is lost");
            }
        }
        let path = self.root.join(key);
        if path.exists() {
            return Ok(false);
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, body)?;
        self.puts.lock().unwrap().push(key.to_string());
        Ok(true)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        Ok(writer::walk(&self.root)?
            .into_keys()
            .filter(|k| k.starts_with(prefix))
            .collect())
    }
}

type Encrypt = fn(&[u8]) -> Result<Vec<u8>>;

/// The reversible encryption of the Python tests, which tests/cross.py uses too.
fn fake_encrypt(data: &[u8]) -> Result<Vec<u8>> {
    Ok(std::iter::once(b'E')
        .chain(data.iter().rev().copied())
        .collect())
}

/// The objects of `proj` for the reader; objects of the fake encryption are encrypted with age
/// on the way.
struct Objects {
    states: PathBuf,
    fake_to: Option<String>,
}

impl Store for Objects {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        let data = std::fs::read(self.states.join(name))?;
        match &self.fake_to {
            Some(recipient) => {
                assert_eq!(data[0], b'E');
                let plain: Vec<u8> = data[1..].iter().rev().copied().collect();
                keys::encrypt(recipient, &plain)
            }
            None => Ok(data),
        }
    }
}

struct Test {
    temp: Temp,
    layr: FakeLayr,
    bucket: FakeBucket,
    identity: String,
}

impl Test {
    fn new(name: &str) -> Test {
        let temp = Temp::new(name);
        let layr = FakeLayr {
            root: temp.0.join("layr"),
        };
        let bucket = FakeBucket {
            root: temp.0.join("objects"),
            puts: Mutex::default(),
            fail: Mutex::default(),
        };
        Test {
            temp,
            layr,
            bucket,
            identity: keys::generate_identity(),
        }
    }

    fn state_dir(&self) -> PathBuf {
        self.temp.0.join("vault")
    }

    fn writer(&self) -> Writer<&FakeLayr, &FakeBucket, impl Fn(&[u8]) -> Result<Vec<u8>>> {
        let recipient = keys::recipient_of(&self.identity).unwrap();
        Writer::new(
            self.state_dir(),
            "proj",
            &self.layr,
            &self.bucket,
            move |data: &[u8]| keys::encrypt(&recipient, data),
        )
        .unwrap()
    }

    /// A writer with the fake encryption of the Python tests.
    fn fake_writer(&self) -> Writer<&FakeLayr, &FakeBucket, Encrypt> {
        Writer::new(
            self.state_dir(),
            "proj",
            &self.layr,
            &self.bucket,
            fake_encrypt as Encrypt,
        )
        .unwrap()
    }

    fn reader(&self, fake: bool) -> Reader<Objects> {
        let states = self.bucket.root.join("proj/states");
        let names: Vec<String> = std::fs::read_dir(&states)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let store = Objects {
            states,
            fake_to: fake.then(|| keys::recipient_of(&self.identity).unwrap()),
        };
        Reader::new(store, &self.identity, &names).unwrap()
    }

    /// The Python writer or reader on the folders of this test.
    fn python(&self, args: &[&str]) -> Value {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cross.py");
        let output = std::process::Command::new("python3")
            .arg(script)
            .args(args)
            .output()
            .expect("python3 runs");
        assert!(
            output.status.success(),
            "cross.py {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn python_push(&self, state: &str) -> Value {
        self.python(&[
            "push",
            self.state_dir().to_str().unwrap(),
            "proj",
            self.layr.root.to_str().unwrap(),
            self.bucket.root.to_str().unwrap(),
            state,
        ])
    }

    fn python_read(&self) -> Vec<Value> {
        let read = self.python(&["read", "proj", self.bucket.root.to_str().unwrap()]);
        read["states"].as_array().unwrap().clone()
    }

    fn header_line(&self, seq: u64, attempt: &str, fake: bool) -> Value {
        let name = desk_core::format::name(seq, attempt, None);
        let data = std::fs::read(self.bucket.root.join("proj/states").join(name)).unwrap();
        let plain = if fake {
            data[1..].iter().rev().copied().collect()
        } else {
            keys::decrypt(&self.identity, &data).unwrap()
        };
        let line = plain.split(|&b| b == b'\n').next().unwrap();
        serde_json::from_slice(line).unwrap()
    }
}

/// The files of one state (the latest when none).
async fn read(reader: &Reader<Objects>, state: Option<(u64, String)>) -> Files {
    let mut out = Files::new();
    for (path, entry) in reader.files(state).await.unwrap() {
        let data = reader.read(&entry).await.unwrap();
        let mode = match entry.mode() {
            "100755" => "100755",
            "120000" => "120000",
            _ => "100644",
        };
        out.insert(path, (data, mode));
    }
    out
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// What the Python reader prints for one state, without the frame kinds.
fn python_files(state: &Value) -> BTreeMap<String, (String, String)> {
    state["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(p, v)| {
            (
                p.clone(),
                (
                    v[0].as_str().unwrap().to_string(),
                    v[1].as_str().unwrap().to_string(),
                ),
            )
        })
        .collect()
}

fn hashed(files: &Files) -> BTreeMap<String, (String, String)> {
    files
        .iter()
        .map(|(p, (d, m))| (p.clone(), (sha(d), m.to_string())))
        .collect()
}

fn pushed(summary: &Summary, seq: u64) {
    assert!(summary.pushed, "{summary:?}");
    assert_eq!(summary.seq, seq, "{summary:?}");
}

#[tokio::test]
async fn pushes_rebuild_every_state_with_small_deltas() {
    let t = Test::new("deltas");
    let source = source();
    let one = files(&[
        ("app.py", &source, "100644"),
        ("run.sh", b"#!/bin/sh\necho hi\n", "100755"),
        ("link", b"app.py", "120000"),
        ("data/empty", b"", "100644"),
    ]);
    t.layr.commit("s1", &one);
    let writer = t.writer();
    pushed(&writer.push("s1").await.unwrap(), 1);
    assert_eq!(read(&t.reader(false), None).await, one);

    let mut two = one.clone();
    let changed = String::from_utf8(source.clone())
        .unwrap()
        .replace(" * 7\n", " * 70\n");
    two.insert("app.py".into(), (changed.into_bytes(), "100644"));
    two.remove("run.sh");
    two.insert("new.txt".into(), (b"new\n".to_vec(), "100644"));
    t.layr.commit("s2", &two);
    pushed(&writer.push("s2").await.unwrap(), 2);
    let reader = t.reader(false);
    assert_eq!(read(&reader, None).await, two);
    let entry = &reader.files(None).await.unwrap()["app.py"];
    let kinds: Vec<&str> = entry.3.iter().map(|l| l.5.as_str()).collect();
    assert_eq!(kinds, ["delta", "full"]);
    // The delta of a one-line change is much smaller than the compressed file.
    let full = zst::compress(&two["app.py"].0, None).unwrap();
    assert!(entry.3[0].4 * 4 < full.len() as u64);
    // The first state stays readable, and parts precede their tree.
    let first = reader.states()[0].clone();
    assert_eq!(read(&reader, Some(first)).await, one);
    let puts = t.bucket.puts.lock().unwrap().clone();
    for (i, key) in puts.iter().enumerate() {
        if let Some(stem) = key.strip_suffix(".tree.age") {
            let part = puts
                .iter()
                .position(|k| *k == format!("{stem}.0000.data.age"));
            assert!(part.is_some_and(|p| p < i), "{key} before its part");
        }
    }
    let again = writer.push("s2").await.unwrap();
    assert!(!again.pushed);
    assert_eq!(again.seq, 2);
}

#[tokio::test]
async fn chains_and_trees_start_again_from_full() {
    let t = Test::new("chains");
    let writer = t.writer();
    let mut content = source();
    for n in 1..40 {
        content.extend(format!("line {n}\n").as_bytes());
        t.layr
            .commit(&format!("s{n}"), &files(&[("a.txt", &content, "100644")]));
        writer.push(&format!("s{n}")).await.unwrap();
    }
    let reader = t.reader(false);
    assert_eq!(
        read(&reader, None).await,
        files(&[("a.txt", &content, "100644")])
    );
    assert!(reader.files(None).await.unwrap()["a.txt"].3.len() <= MAX_DEPTH + 1);
    let mut full = Vec::new();
    for (seq, attempt) in reader.states() {
        if reader.header(seq, &attempt).await.unwrap().tree == "full" {
            full.push(seq);
        }
    }
    assert_eq!(full, [1, 33]);
}

#[tokio::test]
async fn a_lost_record_starts_a_full_push_after_the_last_seq() {
    let t = Test::new("lost");
    let writer = t.writer();
    let source = source();
    t.layr.commit("s1", &files(&[("a.txt", &source, "100644")]));
    writer.push("s1").await.unwrap();
    std::fs::remove_file(t.state_dir().join("writer.json")).unwrap();
    let more = [source.as_slice(), b"more\n"].concat();
    t.layr.commit("s2", &files(&[("a.txt", &more, "100644")]));
    let summary = writer.push("s2").await.unwrap();
    pushed(&summary, 2);
    assert_eq!(summary.note, None);
    let reader = t.reader(false);
    assert_eq!(reader.files(None).await.unwrap()["a.txt"].3[0].5, "full");
    assert_eq!(read(&reader, None).await["a.txt"].0, more);
}

#[tokio::test]
async fn a_failed_upload_before_the_tree_leaves_no_state_and_the_retry_uses_a_new_attempt() {
    let t = Test::new("failed");
    let writer = t.writer();
    let source = source();
    let one = files(&[("a.txt", &source, "100644")]);
    t.layr.commit("s1", &one);
    *t.bucket.fail.lock().unwrap() = Some(".tree.age");
    assert!(writer.push("s1").await.is_err());
    assert_eq!(writer.record().unwrap(), None);
    let failed = t.bucket.puts.lock().unwrap().clone();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].ends_with(".0000.data.age"));
    *t.bucket.fail.lock().unwrap() = None;
    pushed(&writer.push("s1").await.unwrap(), 1);
    let record = writer.record().unwrap().unwrap();
    assert!(!failed[0].contains(&record.attempt));
    let reader = t.reader(false);
    assert_eq!(reader.states(), [(1, record.attempt.clone())]);
    assert_eq!(read(&reader, None).await, one);

    // After a record: the failed push keeps it, and the retry continues from it with deltas.
    let mut two = one.clone();
    two.get_mut("a.txt").unwrap().0.extend(b"two\n");
    t.layr.commit("s2", &two);
    *t.bucket.fail.lock().unwrap() = Some(".tree.age");
    assert!(writer.push("s2").await.is_err());
    assert_eq!(writer.record().unwrap().unwrap(), record);
    *t.bucket.fail.lock().unwrap() = None;
    pushed(&writer.push("s2").await.unwrap(), 2);
    let second = writer.record().unwrap().unwrap();
    assert_eq!(second.since_full, 1);
    let reader = t.reader(false);
    assert_eq!(reader.states().len(), 2);
    assert_eq!(read(&reader, None).await, two);
    let entry = &reader.files(None).await.unwrap()["a.txt"];
    assert_eq!(entry.3[0].1, second.attempt);
    assert_eq!(entry.3[0].5, "delta");
}

#[tokio::test]
async fn a_state_folder_that_does_not_hold_together_gives_a_full_push() {
    let t = Test::new("broken");
    let writer = t.writer();
    let mut content = source();
    let mut push = |n: u32| {
        content.extend(format!("line {n}\n").as_bytes());
        t.layr
            .commit(&format!("s{n}"), &files(&[("a.txt", &content, "100644")]));
        content.clone()
    };
    push(1);
    writer.push("s1").await.unwrap();
    let first_record = std::fs::read(t.state_dir().join("writer.json")).unwrap();
    push(2);
    writer.push("s2").await.unwrap();
    // The Python writer stopped after tree.json and before its record: the tree is newer.
    std::fs::write(t.state_dir().join("writer.json"), &first_record).unwrap();
    let three = push(3);
    let summary = writer.push("s3").await.unwrap();
    pushed(&summary, 3);
    assert!(summary.note.unwrap().contains("newer"));
    let reader = t.reader(false);
    assert_eq!(read(&reader, None).await["a.txt"].0, three);
    assert_eq!(
        reader
            .header(3, &reader.latest().unwrap().1)
            .await
            .unwrap()
            .tree,
        "full"
    );

    // A writer.tmp left by the Python writer, and a missing tree.json.
    std::fs::write(t.state_dir().join("writer.tmp"), b"{\"seq\": 4").unwrap();
    push(4);
    let summary = writer.push("s4").await.unwrap();
    pushed(&summary, 4);
    assert!(summary.note.unwrap().contains("writer.tmp"));
    std::fs::remove_file(t.state_dir().join("tree.json")).unwrap();
    let five = push(5);
    let summary = writer.push("s5").await.unwrap();
    pushed(&summary, 5);
    assert!(summary.note.unwrap().contains("tree.json is missing"));
    assert_eq!(read(&t.reader(false), None).await["a.txt"].0, five);
    // And a whole folder again continues with deltas.
    push(6);
    let summary = writer.push("s6").await.unwrap();
    assert_eq!(summary.note, None);
    assert_eq!(writer.record().unwrap().unwrap().since_full, 1);
}

/// Names with non-ASCII and control characters, which the canonical tree escapes; a tab or a
/// line break also splits a line of `layr diff`.
fn odd_names(version: &[u8]) -> Files {
    let source = source();
    let mut files = Files::new();
    for name in [
        "caf\u{e9}/\u{4e2d}\u{6587}.txt",
        "emoji \u{1F600} \u{10000}",
        "tab\there",
        "new\nline",
        "ctl \u{1}\u{1f}\u{7f}\u{8}\u{c}\r",
        "quote\" back\\slash",
        "sep\u{2028}\u{feff}",
    ] {
        let content = [name.as_bytes(), version, &source].concat();
        files.insert(name.to_string(), (content, "100644"));
    }
    files.insert(
        "link \u{e9}".into(),
        ("caf\u{e9}/\u{4e2d}\u{6587}.txt".into(), "120000"),
    );
    files
}

/// About 3.4 MB of text, beyond the 2 MiB window of zstd level 3.
fn large() -> Vec<u8> {
    let source = source();
    let mut out = Vec::new();
    for i in 0..136 {
        out.extend(format!("# copy {i}\n").as_bytes());
        out.extend(&source);
    }
    out
}

#[tokio::test]
async fn the_python_reader_reads_what_this_writer_pushed() {
    let t = Test::new("python-reads");
    let writer = t.fake_writer();
    let large = large();
    let mut one = odd_names(b" one");
    one.insert("large.txt".into(), (large.clone(), "100644"));
    one.insert("run.sh".into(), (b"#!/bin/sh\n".to_vec(), "100755"));
    t.layr.commit("s1", &one);
    pushed(&writer.push("s1").await.unwrap(), 1);

    // Changed, added and removed names that a diff line cannot carry.
    let mut two = odd_names(b" two");
    two.remove("ctl \u{1}\u{1f}\u{7f}\u{8}\u{c}\r");
    two.insert("added\ttab\nline".into(), (b"added".to_vec(), "100644"));
    let mut edited = large.clone();
    edited.splice(1_700_000..1_700_000, b"an inserted line\n".iter().copied());
    two.insert("large.txt".into(), (edited, "100644"));
    two.insert("run.sh".into(), (b"#!/bin/sh\n".to_vec(), "100644"));
    t.layr.commit("s2", &two);
    let summary = writer.push("s2").await.unwrap();
    pushed(&summary, 2);
    // The large file is a small delta, like `zstd --patch-from` makes it.
    assert!(summary.bytes.unwrap() < 20_000, "{summary:?}");

    let states = t.python_read();
    assert_eq!(states.len(), 2);
    for (state, want) in states.iter().zip([&one, &two]) {
        assert_eq!(state["canonical"], true);
        assert_eq!(python_files(state), hashed(want));
    }
    assert_eq!(states[1]["header"]["tree"], "delta");
    assert_eq!(
        states[1]["files"]["large.txt"][2],
        serde_json::json!(["delta", "full"])
    );
    assert_eq!(
        states[1]["files"]["tab\there"][2],
        serde_json::json!(["delta", "full"])
    );
    // desk-core reads the same states.
    let reader = t.reader(true);
    for ((seq, attempt), want) in reader.states().into_iter().zip([&one, &two]) {
        assert_eq!(&read(&reader, Some((seq, attempt))).await, want);
    }
}

#[tokio::test]
async fn each_writer_continues_a_state_folder_of_the_other() {
    let t = Test::new("takeover");
    let source = source();
    let state = |n: u32| {
        // The Python writer cannot see a changed or added name with a tab or a line break after
        // its first push, so those stay as the first state has them.
        let mut files = odd_names(b" one");
        let mut app = source.clone();
        for i in 1..=n {
            app.extend(format!("line {i}\n").as_bytes());
        }
        files.insert("app.py".into(), (app, "100644"));
        files.insert(
            format!("added {n} \u{e9}"),
            (vec![b'x'; n as usize], "100644"),
        );
        files
    };
    for n in 1..=6 {
        t.layr.commit(&format!("s{n}"), &state(n));
    }
    assert_eq!(t.python_push("s1")["seq"], 1);
    assert_eq!(t.python_push("s2")["seq"], 2);
    let python_tree = std::fs::read(t.state_dir().join("tree.json")).unwrap();

    let writer = t.fake_writer();
    let summary = writer.push("s3").await.unwrap();
    pushed(&summary, 3);
    assert_eq!(summary.note, None);
    pushed(&writer.push("s4").await.unwrap(), 4);
    let record = writer.record().unwrap().unwrap();
    assert_eq!(record.since_full, 3);
    // The Python writer continues this writer's folder in turn.
    assert_eq!(t.python_push("s5")["seq"], 5);
    pushed(&writer.push("s6").await.unwrap(), 6);

    // Delta trees and file chains run across both writers.
    let states = t.python_read();
    assert_eq!(states.len(), 6);
    for (n, state) in states.iter().enumerate() {
        assert_eq!(state["canonical"], true);
        assert_eq!(python_files(state), hashed(&state_files(&t, n + 1)));
        let want = if n == 0 { "full" } else { "delta" };
        assert_eq!(state["header"]["tree"], want, "state {}", n + 1);
    }
    assert_eq!(
        states[5]["files"]["app.py"][2],
        serde_json::json!(["delta", "delta", "delta", "delta", "delta", "full"])
    );
    let third = t.header_line(3, states[2]["attempt"].as_str().unwrap(), true);
    assert_eq!(third["base"], serde_json::json!([2, states[1]["attempt"]]));
    // The tree that the Python writer kept is the canonical tree this writer reads.
    let reader = t.reader(true);
    let (_, second) = reader
        .tree(2, states[1]["attempt"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(*second, python_tree);
    for (n, state) in reader.states().into_iter().enumerate() {
        assert_eq!(read(&reader, Some(state)).await, state_files(&t, n + 1));
    }
}

fn state_files(t: &Test, n: usize) -> Files {
    t.layr.state(&format!("s{n}"))
}
