//! Ids. Records and states use UUIDv7. A state is shown as 32 hex digits that start with the
//! random part of the UUID, so short prefixes are unique like git hashes.

use uuid::Uuid;

pub fn new_uuid() -> Uuid {
    Uuid::now_v7()
}

pub fn new_id() -> String {
    new_uuid().hyphenated().to_string()
}

/// Display form: bytes 10..16, 6..10, 0..6 of the UUID in hex. A bijection with the UUID.
pub fn display(id: &str) -> String {
    match Uuid::parse_str(id) {
        Ok(u) => {
            let b = u.as_bytes();
            let mut v = Vec::with_capacity(16);
            v.extend_from_slice(&b[10..16]);
            v.extend_from_slice(&b[6..10]);
            v.extend_from_slice(&b[0..6]);
            hex::encode(v)
        }
        Err(_) => id.to_string(),
    }
}

pub fn short(id: &str) -> String {
    display(id)[..7].to_string()
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
