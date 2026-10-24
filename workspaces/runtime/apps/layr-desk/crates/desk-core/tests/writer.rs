//! The writer against fake layr state folders and bucket objects. The reader rebuilds
//! every pushed state, checks small deltas and odd names, and verifies signed trees.

use anyhow::{bail, Result};
use desk_core::sign::Signer;
use desk_core::writer::{self, Bucket, Layr, Summary, Writer, FULL_TREE_EVERY, MAX_DEPTH};
use desk_core::{keys, zst, Policy, Reader, Store, VaultInfo};
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

/// Reversible test encryption for inspecting fake bucket objects.
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
        self.reader_with(fake, None, None)
    }

    /// The reader of a vault.json that names `writer` from `signed_from` on.
    fn reader_with(
        &self,
        fake: bool,
        writer: Option<&Signer>,
        signed_from: Option<u64>,
    ) -> Reader<Objects> {
        let policy = Policy::new(
            "proj",
            &VaultInfo {
                recipient: keys::recipient_of(&self.identity).unwrap(),
                writer: writer.map(Signer::public),
                signed_from,
            },
        )
        .unwrap();
        let states = self.bucket.root.join("proj/states");
        let names: Vec<String> = std::fs::read_dir(&states)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let store = Objects {
            states,
            fake_to: fake.then(|| keys::recipient_of(&self.identity).unwrap()),
        };
        Reader::new(store, &self.identity, &names, policy).unwrap()
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
    let writer = t.fake_writer();
    let mut content = source();
    let last = FULL_TREE_EVERY as usize + 1;
    for n in 1..=last {
        content.extend(format!("line {n}\n").as_bytes());
        t.layr
            .commit(&format!("s{n}"), &files(&[("a.txt", &content, "100644")]));
        writer.push(&format!("s{n}")).await.unwrap();
        if n == MAX_DEPTH + 1 || n == MAX_DEPTH + 2 || n == last {
            let reader = t.reader(true);
            assert_eq!(read(&reader, None).await["a.txt"].0, content);
            let depth = (n - 1) % (MAX_DEPTH + 1) + 1;
            assert_eq!(reader.files(None).await.unwrap()["a.txt"].3.len(), depth);
        }
    }
    let reader = t.reader(true);
    for (seq, attempt) in reader.states() {
        let full = reader.header(seq, &attempt).await.unwrap().tree == "full";
        assert_eq!(full, seq == 1 || seq == FULL_TREE_EVERY + 1);
    }
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

fn signer(byte: u8) -> Signer {
    use base64::Engine;
    Signer::from_base64(&base64::engine::general_purpose::STANDARD.encode([byte; 32])).unwrap()
}

#[tokio::test]
async fn a_writer_with_a_key_signs_each_tree_from_its_first_push_on() {
    let t = Test::new("signed");
    let source = source();
    let state = |n: u64| {
        let mut app = source.clone();
        app.extend(format!("line {n}\n").as_bytes());
        files(&[
            ("app.py", &app, "100644"),
            ("n.txt", n.to_string().as_bytes(), "100644"),
        ])
    };
    // Two pushes before the writer had a key, then three with it.
    for n in 1..=5 {
        t.layr.commit(&format!("s{n}"), &state(n));
    }
    let unsigned = t.writer();
    pushed(&unsigned.push("s1").await.unwrap(), 1);
    pushed(&unsigned.push("s2").await.unwrap(), 2);
    let key = signer(5);
    let writer = t.writer().signed_by(signer(5));
    for n in 3..=5u64 {
        pushed(&writer.push(&format!("s{n}")).await.unwrap(), n);
    }
    let reader = t.reader_with(false, Some(&key), Some(3));
    for (seq, attempt) in reader.states() {
        assert_eq!(reader.signed(seq), seq >= 3);
        let header = reader.header(seq, &attempt).await.unwrap();
        assert_eq!(header.sig.is_some(), seq >= 3, "state {seq}");
        let want = state(seq);
        assert_eq!(read(&reader, Some((seq, attempt))).await, want);
    }
    // The same states under another key, or with every state required to be signed.
    let other = t.reader_with(false, Some(&signer(6)), Some(3));
    let err = other.files(None).await.unwrap_err().to_string();
    assert!(err.contains("does not match"), "{err}");
    let strict = t.reader_with(false, Some(&key), None);
    let err = strict.files(Some((1, strict.states()[0].1.clone()))).await;
    assert!(err.unwrap_err().to_string().contains("no writer signature"));
    assert!(
        strict.files(None).await.is_err(),
        "state 5 rests on state 2"
    );
}

#[tokio::test]
async fn signed_writer_states_are_readable_with_odd_names_and_small_deltas() {
    let t = Test::new("signed-odd-names");
    // Signed trees: the Python reader ignores the signature, desk-core checks it.
    let key = signer(9);
    let writer = t.fake_writer().signed_by(signer(9));
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

    // desk-core reads the same states.
    let reader = t.reader_with(true, Some(&key), Some(1));
    let states = reader.states();
    assert_eq!(states.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(reader.latest().unwrap(), states[1]);
    for ((seq, attempt), want) in states.into_iter().zip([&one, &two]) {
        assert_eq!(&read(&reader, Some((seq, attempt))).await, want);
    }
}
