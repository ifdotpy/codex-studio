//! Captured legacy expectations; fixture readers never execute backend code.
use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;
use serde_json::Value;

/// Small fixture syntax keeps boundary strings and repeated snapshots reviewable.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    values: Vec<FixtureValue>,
    root: FixtureValue,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum FixtureValue {
    // Serde also accepts tuples for untagged structs; keep arrays unambiguous.
    Array(Vec<FixtureValue>),
    Reference(Reference),
    Extend(Extend),
    Repeat(Repeat),
    Concat(Concat),
    Object(BTreeMap<String, FixtureValue>),
    Scalar(Scalar),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    #[serde(rename = "$ref")]
    index: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Extend {
    #[serde(rename = "$base")]
    index: usize,
    set: BTreeMap<String, FixtureValue>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repeat {
    #[serde(rename = "$repeat")]
    text: String,
    count: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Concat {
    #[serde(rename = "$concat")]
    parts: Vec<FixtureValue>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Text(String),
    Number(serde_json::Number),
    Boolean(bool),
    Null(()),
}
impl FixtureValue {
    fn expand(&self, values: &[Value]) -> Value {
        match self {
            Self::Reference(v) => values
                .get(v.index)
                .expect("preceding fixture definition")
                .clone(),
            Self::Extend(v) => {
                let Value::Object(mut fields) = values
                    .get(v.index)
                    .expect("preceding object definition")
                    .clone()
                else {
                    panic!("fixture base must be an object")
                };
                for (key, value) in &v.set {
                    fields.insert(key.clone(), value.expand(values));
                }
                Value::Object(fields)
            }
            Self::Repeat(v) => Value::String(v.text.repeat(v.count)),
            Self::Concat(v) => Value::String(
                v.parts
                    .iter()
                    .map(|p| match p.expand(values) {
                        Value::String(s) => s,
                        _ => panic!("fixture concatenation requires text"),
                    })
                    .collect(),
            ),
            Self::Array(v) => Value::Array(v.iter().map(|v| v.expand(values)).collect()),
            Self::Object(v) => Value::Object(
                v.iter()
                    .map(|(k, v)| (k.clone(), v.expand(values)))
                    .collect(),
            ),
            Self::Scalar(Scalar::Text(v)) => Value::String(v.clone()),
            Self::Scalar(Scalar::Number(v)) => Value::Number(v.clone()),
            Self::Scalar(Scalar::Boolean(v)) => Value::Bool(*v),
            Self::Scalar(Scalar::Null(())) => Value::Null,
        }
    }
}
pub(crate) fn load<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> T {
    let fixture: Fixture = serde_json::from_slice(bytes).expect("typed compact JSON fixture");
    let mut values = Vec::new();
    for value in fixture.values {
        values.push(value.expand(&values));
    }
    serde_json::from_value(fixture.root.expand(&values)).expect("typed frozen JSON fixture")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DatabaseSeed {
    schema: Vec<String>,
    data: Vec<String>,
}

impl DatabaseSeed {
    pub(crate) fn write(&self, path: &Path) {
        write_database(path, &self.schema, &self.data);
    }
}

pub(crate) fn write_database(path: &Path, schema: &[String], data: &[String]) {
    let mut connection = rusqlite::Connection::open(path).expect("fixture database");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("fixture WAL mode");
    let tx = connection.transaction().expect("fixture transaction");
    // Install triggers after the captured rows; running them while loading would
    // generate new revisions and corrupt the captured prestate.
    for sql in schema
        .iter()
        .filter(|sql| !sql.starts_with("CREATE TRIGGER"))
    {
        tx.execute_batch(sql).expect("captured schema");
    }
    for sql in data {
        tx.execute_batch(sql).expect("captured row");
    }
    // Rebinding captured paths changes content-addressed payloads. Rehash them
    // before installing triggers so the seed keeps its captured revisions.
    let has_entities: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='sync_entities')",
            [],
            |row| row.get(0),
        )
        .expect("sync table presence");
    if has_entities {
        let mut statement = tx
            .prepare("SELECT rowid,payload FROM sync_entities")
            .expect("seed sync payloads");
        let rows: Vec<(i64, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("seed sync rows")
            .collect::<Result<_, _>>()
            .expect("seed sync values");
        for (rowid, payload) in rows {
            tx.execute(
                "UPDATE sync_entities SET hash=?1 WHERE rowid=?2",
                rusqlite::params![studio_runtime_store::sha256_hex(payload.as_bytes()), rowid],
            )
            .expect("rebind seed content hash");
        }
    }
    for sql in schema
        .iter()
        .filter(|sql| sql.starts_with("CREATE TRIGGER"))
    {
        tx.execute_batch(sql).expect("captured trigger");
    }
    tx.commit().expect("fixture commit");
}

pub(crate) fn write_state(bytes: &[u8], root: &Path) {
    let databases: BTreeMap<String, DatabaseSeed> = load(bytes);
    for (name, database) in databases {
        database.write(&root.join(name));
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedCase {
    case: Value,
    database: DatabaseSeed,
}

pub(crate) fn database_cases(bytes: &[u8], root: &Path) -> Vec<Value> {
    let cases: Vec<CapturedCase> = load(bytes);
    cases
        .into_iter()
        .map(|mut captured| {
            let path = root.join(captured.case["path"].as_str().expect("fixture filename"));
            captured.database.write(&path);
            captured.case["path"] = serde_json::json!(path);
            captured.case
        })
        .collect()
}

pub(crate) struct FrozenResponses(BTreeMap<String, Value>);
impl FrozenResponses {
    pub(crate) fn new(bytes: &[u8]) -> Self {
        Self(load(bytes))
    }
    pub(crate) fn call(&self, request: &Value) -> Value {
        let key = studio_runtime_store::sha256_hex(request.to_string().as_bytes());
        self.0
            .get(&key)
            .unwrap_or_else(|| panic!("uncaptured fixture request: {request}"))
            .clone()
    }
}
