//! Native first-page benchmark. Generation and encryption are outside the measured interval.
//! The indexed interval authenticates the provisional snapshot; record reconciliation remains
//! background work, exactly as specified by the browser flow.
use anyhow::{ensure, Result};
use ed25519_dalek::SigningKey;
use layr_model::{index as model_index, Model, Record};
use layr_store::{index, keys, record};
use serde_json::json;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

const MACHINE: &str = "01a12290-0000-7000-8000-000000000001";
const PROJECT: &str = "01a12290-0000-7000-8000-000000000002";
const LINE: &str = "01a12290-0000-7000-8000-000000000003";
const STATE: &str = "01a12290-0000-7000-8000-000000000004";

fn line(seq: usize, key: &SigningKey) -> String {
    let mut r = Record::new(match seq {
        1 => "machine",
        2 => "project",
        _ => "note",
    });
    r.id = format!("01a12290-0000-7000-8001-{seq:012}");
    r.machine = MACHINE.into();
    r.project = PROJECT.into();
    r.seq = seq as u64;
    r.time = seq as i64;
    r.actor.name = "Benchmark".into();
    if seq == 1 {
        r.data = json!({"public_key":hex::encode(key.verifying_key().to_bytes())});
    }
    if seq == 2 {
        r.data = json!({"name":"bench","config":{}});
        r.states.push(
            serde_json::from_value(json!({"id":STATE,"kind":"commit","subvol":STATE,
            "author":{"name":"Benchmark","email":"bench@example.test"},"message":"initial state","time":2}))
            .unwrap(),
        );
        r.lines.insert(
            LINE.into(),
            serde_json::from_value(json!({"before":null,"after":{
            "name":"main","head":STATE,"owner":0,"machine":MACHINE}}))
            .unwrap(),
        );
    }
    let body = serde_json::to_string(&r).unwrap();
    format!(r#"{{"r":{body},"s":"{}"}}"#, keys::sign_bytes(body.as_bytes(), key))
}
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() -> Result<()> {
    let sizes = std::env::args().skip(1).map(|s| s.parse()).collect::<std::result::Result<Vec<usize>, _>>()?;
    let sizes = if sizes.is_empty() { vec![1_000, 100_000, 1_000_000] } else { sizes };
    let identity = keys::generate_identity();
    let recipient = keys::recipient_of(&identity)?;
    let key = SigningKey::from_bytes(&[7; 32]);
    let trusted = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
    for count in sizes {
        ensure!(count >= 2, "at least two records");
        let mut segments = Vec::new();
        let mut segment = String::new();
        let mut last = String::new();
        let initial = [line(1, &key), line(2, &key)];
        for seq in 1..=count {
            last = line(seq, &key);
            segment.push_str(&last);
            segment.push('\n');
            if seq % 1000 == 0 || seq == count {
                segments.push(keys::encrypt(&recipient, segment.as_bytes())?);
                segment.clear();
            }
        }
        let model = Model::from_verified_lines(initial.iter().map(String::as_str), "")?;
        let mark = BTreeMap::from([(MACHINE.into(), index::Watermark { seq: count as u64, hash: index::line_hash(&last) })]);
        let mut heads = model_index::heads(&model.projects[PROJECT], mark, 1)?;
        heads.counts.records = count as u64;
        let snapshot =
            index::Index { v: 1, project: PROJECT.into(), machine: MACHINE.into(), data: index::Data::Heads(heads.clone()) };
        let encrypted = snapshot.seal(&recipient, &key)?;
        let mut before = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            let mut verified = Vec::with_capacity(count);
            for segment in &segments {
                let plain = String::from_utf8(keys::decrypt(&identity, segment)?)?;
                for line in plain.lines() {
                    record::verify_line(line.as_bytes(), &key.verifying_key())?;
                    verified.push(line.to_string());
                }
            }
            let full = Model::from_verified_lines(verified.iter().map(String::as_str), "")?;
            let actual = model_index::heads(&full.projects[PROJECT], heads.watermark.clone(), 1)?;
            ensure!(actual == heads, "snapshot differs from full verified first page");
            black_box(&actual);
            before.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let mut after = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            let opened = index::open(&identity, &encrypted, &snapshot.name(), PROJECT, &trusted)?;
            black_box(opened);
            after.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let before_ms = median(&mut before);
        let after_ms = median(&mut after);
        println!(
            "{}",
            json!({"records":count,"before_ms":before_ms,"after_ms":after_ms,
            "before_samples_ms":before,"after_samples_ms":after,"snapshot_bytes":encrypted.len(),
            "record_bytes":segments.iter().map(Vec::len).sum::<usize>(),"speedup":before_ms/after_ms,
            "measurement":"in-memory encrypted objects, release native reader, authenticated snapshot before background reconciliation"})
        );
    }
    Ok(())
}
