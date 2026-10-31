//! Move provider read replies (history pages) out of account transfer records.
//! A record keeps the receipt and a digest; the reply body is a payload blob.
use crate::{MAX_INLINE_READ_REPLY_BYTES, StoreResult, sha256_hex};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    if !crate::schema::table_exists(tx, "runtime_account_transfers")? {
        return Ok(());
    }
    let Some(root) = tx.path().and_then(|p| Path::new(p).parent()) else {
        return Ok(());
    };
    let mut query = tx.prepare(
        "SELECT id,record FROM runtime_account_transfers WHERE json_valid(record) AND length(record)>?1",
    )?;
    let rows = query
        .query_map([MAX_INLINE_READ_REPLY_BYTES as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut moved = 0u64;
    for (id, raw) in rows {
        let record: Value = serde_json::from_str(&raw)?;
        let Some(members) = record["members"].as_object() else {
            continue;
        };
        for (member, value) in members {
            let Some(receipts) = value["providerReadReceipts"].as_object() else {
                continue;
            };
            for (operation, receipt) in receipts {
                let Some(body) = receipt.pointer("/result/value") else {
                    continue;
                };
                let bytes = serde_json::to_vec(body)?;
                if bytes.len() <= MAX_INLINE_READ_REPLY_BYTES {
                    continue;
                }
                let digest = sha256_hex(&bytes);
                write_blob(root, &digest, &bytes)?;
                let reference = json!({"sha256":digest,"size":bytes.len()});
                let path =
                    format!("$.members.\"{member}\".providerReadReceipts.\"{operation}\".result");
                tx.execute(
                    "UPDATE runtime_account_transfers SET record=json_remove(json_set(record,?2||'.valueBlob',json(?3)),?2||'.value') WHERE id=?1",
                    params![id, path, reference.to_string()],
                )?;
                moved += 1;
            }
        }
    }
    eprintln!("transfer history pages migration: moved={moved}");
    Ok(())
}

fn write_blob(root: &Path, digest: &str, bytes: &[u8]) -> StoreResult<()> {
    use std::io::Write;
    let directory = root.join("blobs").join(&digest[..2]).join(&digest[2..4]);
    std::fs::create_dir_all(&directory)?;
    let target = directory.join(digest);
    if target.exists() {
        return Ok(());
    }
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&target) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn moves_large_replies_to_blobs_and_keeps_member_order() {
        let root = tempfile::tempdir().unwrap();
        let mut db = Connection::open(root.path().join("canvas.sqlite3")).unwrap();
        db.execute_batch(
            "CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
        )
        .unwrap();
        let big = json!({"data":["x".repeat(MAX_INLINE_READ_REPLY_BYTES)]});
        let receipt = |value: &Value| json!({"certainty":"written","receipt":{"operation":"o"},"result":{"kind":"reply","value":value}});
        let z = json!({"providerReadReceipts":{"turns:0":receipt(&big),"small":receipt(&json!({"data":[]}))}});
        let text = format!("{{\"id\":\"t\",\"members\":{{\"z\":{z},\"a\":{{}}}}}}");
        db.execute(
            "INSERT INTO runtime_account_transfers VALUES('t',?1)",
            [text],
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        apply(&tx).unwrap();
        apply(&tx).unwrap();
        let raw: String = tx
            .query_row("SELECT record FROM runtime_account_transfers", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(raw.len() < MAX_INLINE_READ_REPLY_BYTES);
        let moved: Value = serde_json::from_str(&raw).unwrap();
        let page = &moved["members"]["z"]["providerReadReceipts"]["turns:0"];
        assert_eq!(page["receipt"]["operation"], "o");
        assert!(page["result"].get("value").is_none());
        let bytes = serde_json::to_vec(&big).unwrap();
        assert_eq!(page["result"]["valueBlob"]["sha256"], sha256_hex(&bytes));
        assert_eq!(
            std::fs::read(
                root.path()
                    .join("blobs")
                    .join(&sha256_hex(&bytes)[..2])
                    .join(&sha256_hex(&bytes)[2..4])
                    .join(sha256_hex(&bytes))
            )
            .unwrap(),
            bytes
        );
        assert_eq!(
            moved["members"]["z"]["providerReadReceipts"]["small"]["result"]["value"],
            json!({"data":[]})
        );
        assert!(raw.find("\"z\"") < raw.find("\"a\""));
    }
}
