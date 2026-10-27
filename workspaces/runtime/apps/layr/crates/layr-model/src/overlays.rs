//! Public overlay metadata from verified records. Key material is never returned.

use crate::Record;
use serde_json::{json, Value};

/// Number signed overlay key records in the model's canonical application order.
/// The caller supplies records from one overlay and verifies signatures before calling.
/// Arrival order, UUID spelling, and operation-log display order do not affect numbering.
/// A repeated epoch ID retains a separate entry for each signed key record.
pub fn epochs(records: &[Record]) -> Vec<Value> {
    let mut keys: Vec<_> = records.iter().filter(|r| r.op == "overlay.key").collect();
    keys.sort_by(|a, b| crate::order(a).cmp(&crate::order(b)));
    keys.into_iter()
        .enumerate()
        .map(|(index, r)| {
            let mut row = json!({"number":index + 1,"id":r.data["epoch"],"time":r.time,"machine":r.machine});
            if let Some(reason) = r.data.get("reason") {
                row["reason"] = reason.clone();
            }
            row
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_signed_key_records_and_reasons_are_not_invented() {
        let mut first = Record::new("overlay.key");
        first.time = 100;
        first.machine = "manager".into();
        first.data = json!({"epoch":"z-first","recipient":"key material"});
        let mut second = first.clone();
        second.time = 200;
        second.data = json!({"epoch":"a-second","reason":"reader removed","identity":"secret"});
        let mut refresh = second.clone();
        refresh.time = 300;
        refresh.data = json!({"epoch":"a-second"});
        let records = [refresh, Record::new("project"), second, Record::new("config"), first];
        assert_eq!(
            epochs(&records),
            vec![
                json!({"number":1,"id":"z-first","time":100,"machine":"manager"}),
                json!({"number":2,"id":"a-second","time":200,"machine":"manager","reason":"reader removed"}),
                json!({"number":3,"id":"a-second","time":300,"machine":"manager"}),
            ]
        );
    }

    #[test]
    fn equal_times_use_canonical_machine_then_sequence_order() {
        let key = |machine: &str, seq: u64, epoch: &str| {
            let mut record = Record::new("overlay.key");
            record.time = 100;
            record.machine = machine.into();
            record.seq = seq;
            record.data = json!({"epoch":epoch});
            record
        };
        // The second machine has the lowest sequence; sequence alone would put it first.
        let records = [key("machine-b", 1, "a-epoch"), key("machine-a", 9, "b-epoch"), key("machine-a", 2, "z-epoch")];
        assert_eq!(
            epochs(&records),
            vec![
                json!({"number":1,"id":"z-epoch","time":100,"machine":"machine-a"}),
                json!({"number":2,"id":"b-epoch","time":100,"machine":"machine-a"}),
                json!({"number":3,"id":"a-epoch","time":100,"machine":"machine-b"}),
            ]
        );
    }
}
