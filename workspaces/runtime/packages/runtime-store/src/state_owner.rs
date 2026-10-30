//! The Rust state-owner marker: `<state>/rust-state-owner.json`.
//!
//! Python's `codex_state.state_dir()` refuses a state directory with this marker unless its
//! `(formatVersion, backendBuild)` pair is in `PYTHON_COMPATIBLE_RUST_STATE_OWNERS`. The Rust
//! server writes it while it holds `runtime.lock` and before its first write to the state, so a
//! Rust launch that never reached the state leaves Python able to open it, and every entry point
//! (desktop, launchd, systemd, a direct start) is fenced the same way.
//!
//! `formatVersion` names the state format that Rust writes. A later Rust build with the same
//! format takes over the marker and records its own build; an unknown runtime, a malformed
//! marker or another format version is refused.
use crate::{StoreError, StoreResult};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

/// File name of the marker in the state directory. `codex_state.py` reads the same name.
pub const STATE_OWNER_FILE: &str = "rust-state-owner.json";
/// State format written by this Rust build. `codex_state.py` compares this value.
pub const STATE_FORMAT_VERSION: i64 = 1;
/// Temporary file of an atomic marker write.
pub(crate) const STATE_OWNER_TEMPORARY_FILE: &str = ".rust-state-owner.tmp";

/// What the Rust server records as the owner of a state directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateOwnerClaim {
    /// Lowercase SHA-256 of the running executable (the `/api/desktop` backend build).
    pub backend_build: String,
}

// The marker is an object on disk; serde's derived struct also accepts arrays.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateOwnerMarker {
    backend_build: String,
    format_version: i64,
    runtime: String,
}

fn decode_marker(bytes: &[u8]) -> Result<StateOwnerMarker, serde_json::Error> {
    struct Object;
    impl<'de> serde::de::Visitor<'de> for Object {
        type Value = StateOwnerMarker;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a state owner object")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            StateOwnerMarker::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let marker = deserializer.deserialize_map(Object)?;
    deserializer.end()?;
    Ok(marker)
}

/// Record `claim` as the owner of `root`. The caller holds `runtime.lock`.
pub(crate) fn claim(root: &Path, claim: &StateOwnerClaim) -> StoreResult<()> {
    if claim.backend_build.len() != 64
        || !claim
            .backend_build
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(owner_error("the backend build is not a lowercase SHA-256"));
    }
    let marker = root.join(STATE_OWNER_FILE);
    match fs::read(&marker) {
        Ok(bytes) => {
            let prior = decode_marker(&bytes).map_err(|error| match error.classify() {
                serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                    owner_error("the state owner marker is malformed")
                }
                serde_json::error::Category::Data | serde_json::error::Category::Io => {
                    owner_error("the state directory has an unknown owner or another state format")
                }
            })?;
            if prior.runtime != "rust" || prior.format_version != STATE_FORMAT_VERSION {
                return Err(owner_error(
                    "the state directory has an unknown owner or another state format",
                ));
            }
            if prior.backend_build == claim.backend_build {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(StoreError::StateDirectory(error)),
    }
    write_marker(root, &marker, &claim.backend_build).map_err(StoreError::StateDirectory)
}

/// Replace the marker atomically: write and sync a private temporary file, rename it over the
/// marker, then sync the directory so the rename survives a power loss.
fn write_marker(root: &Path, marker: &Path, backend_build: &str) -> io::Result<()> {
    // One fixed name: the caller holds runtime.lock, and a file left by a killed claim is
    // replaced here instead of blocking the next start.
    let temporary = root.join(STATE_OWNER_TEMPORARY_FILE);
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut body = serde_json::to_vec(&StateOwnerMarker {
        backend_build: backend_build.to_owned(),
        format_version: STATE_FORMAT_VERSION,
        runtime: "rust".to_owned(),
    })
    .map_err(io::Error::other)?;
    body.push(b'\n');
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode_private()
            .open(&temporary)?;
        file.write_all(&body)?;
        file.sync_all()?;
        fs::rename(&temporary, marker)?;
        fs::File::open(root)?.sync_all()
    })();
    if result.is_err() && fs::remove_file(&temporary).is_err() {
        // The next claim replaces a temporary file that could not be removed here.
    }
    result
}

fn owner_error(reason: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("{STATE_OWNER_FILE}: {reason}"))
}

trait PrivateMode {
    fn mode_private(&mut self) -> &mut Self;
}

impl PrivateMode for fs::OpenOptions {
    #[cfg(unix)]
    fn mode_private(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.mode(0o600)
    }
    #[cfg(not(unix))]
    fn mode_private(&mut self) -> &mut Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn build(byte: char) -> StateOwnerClaim {
        StateOwnerClaim {
            backend_build: byte.to_string().repeat(64),
        }
    }

    fn marker(root: &Path) -> Value {
        serde_json::from_slice(&fs::read(root.join(STATE_OWNER_FILE)).expect("test fixture"))
            .expect("test fixture")
    }

    #[test]
    fn a_first_claim_writes_the_marker_and_a_repeat_keeps_it() {
        let root = tempfile::tempdir().expect("test fixture");
        claim(root.path(), &build('a')).expect("test fixture");
        claim(root.path(), &build('a')).expect("test fixture");
        assert_eq!(
            marker(root.path()),
            json!({"runtime":"rust","formatVersion":1,"backendBuild":"a".repeat(64)})
        );
        assert_eq!(fs::read_dir(root.path()).expect("test fixture").count(), 1);
    }

    #[test]
    fn a_later_build_with_the_same_format_takes_over_the_marker() {
        let root = tempfile::tempdir().expect("test fixture");
        claim(root.path(), &build('a')).expect("test fixture");
        claim(root.path(), &build('b')).expect("test fixture");
        assert_eq!(marker(root.path())["backendBuild"], "b".repeat(64));
    }

    #[test]
    fn marker_shape_and_open_fields_preserve_owner_fencing() {
        let root = tempfile::tempdir().expect("test fixture");
        let marker_path = root.path().join(STATE_OWNER_FILE);
        let valid = json!({"runtime":"rust", "formatVersion":1,
            "backendBuild":"a".repeat(64), "future":{"other":true}})
        .to_string();
        fs::write(&marker_path, &valid).expect("test fixture");
        claim(root.path(), &build('a')).expect("same owner accepts unknown fields");
        assert_eq!(
            fs::read_to_string(&marker_path).expect("test fixture"),
            valid
        );
        for invalid in [
            json!(["a".repeat(64), 1, "rust"]),
            json!({"runtime":"rust","formatVersion":1.0,"backendBuild":"a".repeat(64)}),
            json!({"runtime":"rust","formatVersion":1,"backendBuild":null}),
            json!({"runtime":null,"formatVersion":1,"backendBuild":"a".repeat(64)}),
        ] {
            let raw = invalid.to_string();
            fs::write(&marker_path, &raw).expect("test fixture");
            let error = claim(root.path(), &build('b')).expect_err("invalid marker");
            assert!(
                error
                    .to_string()
                    .contains("unknown owner or another state format")
            );
            assert_eq!(fs::read_to_string(&marker_path).expect("test fixture"), raw);
        }
        fs::write(&marker_path, b"{").expect("test fixture");
        assert!(
            claim(root.path(), &build('b'))
                .expect_err("malformed marker")
                .to_string()
                .contains("state owner marker is malformed")
        );
    }

    #[test]
    fn unknown_owners_formats_and_builds_are_refused_without_a_change() {
        let root = tempfile::tempdir().expect("test fixture");
        for prior in [
            "{".to_owned(),
            json!({"runtime":"python","formatVersion":1,"backendBuild":"a".repeat(64)}).to_string(),
            json!({"runtime":"rust","formatVersion":2,"backendBuild":"a".repeat(64)}).to_string(),
            json!({"runtime":"rust","formatVersion":1}).to_string(),
        ] {
            fs::write(root.path().join(STATE_OWNER_FILE), &prior).expect("test fixture");
            assert!(claim(root.path(), &build('b')).is_err(), "{prior}");
            assert_eq!(
                fs::read_to_string(root.path().join(STATE_OWNER_FILE)).expect("test fixture"),
                prior
            );
        }
        fs::remove_file(root.path().join(STATE_OWNER_FILE)).expect("test fixture");
        let invalid = StateOwnerClaim {
            backend_build: "A".repeat(64),
        };
        assert!(claim(root.path(), &invalid).is_err());
        assert!(!root.path().join(STATE_OWNER_FILE).exists());
    }
}
