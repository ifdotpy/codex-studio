use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha1::Digest as Sha1Digest;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

const FIXTURE_SALT: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

fn origin(scheme: &str, host: &str) -> Origin {
    Origin {
        scheme: scheme.into(),
        host: host.into(),
        port: None,
    }
}
fn root_pair() -> OriginPair {
    let value = origin("tauri", "localhost");
    OriginPair {
        top: value.clone(),
        client: value,
    }
}

// Independent anonymous encoder follows the pinned WebKit persistence grammar.
// No installed metadata, salt, or user payload is part of this fixture.
fn encoded(pair: &OriginPair) -> Vec<u8> {
    let mut bytes = vec![];
    for origin in [&pair.top, &pair.client] {
        for value in [&origin.scheme, &origin.host] {
            let units: Vec<_> = value.encode_utf16().collect();
            bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
            let latin1 = units.iter().all(|unit| *unit <= 255);
            bytes.push(u8::from(latin1));
            for unit in units {
                if latin1 {
                    bytes.push(unit as u8);
                } else {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
            }
        }
        bytes.push(u8::from(origin.port.is_some()));
        if let Some(port) = origin.port {
            bytes.extend_from_slice(&port.to_le_bytes());
        }
    }
    bytes
}
fn hash(value: &Origin) -> String {
    let text = if value.scheme == "file" {
        "file://".to_owned()
    } else {
        format!(
            "{}://{}{}",
            value.scheme,
            value.host,
            value
                .port
                .map(|port| format!(":{port}"))
                .unwrap_or_default()
        )
    };
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    digest.update(FIXTURE_SALT);
    URL_SAFE_NO_PAD.encode(digest.finalize())
}
fn cache_hash(pair: &OriginPair) -> String {
    // NetworkCache::Key::computeHash and hashAsString own this legacy contract.
    let mut digest = sha1::Sha1::new();
    digest.update(FIXTURE_SALT);
    for origin in [&pair.top, &pair.client] {
        digest.update(format!("{}://{}", origin.scheme, origin.host).as_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}
fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    _directory: tempfile::TempDir,
    selection: TrustedStoreSelection,
    general: PathBuf,
    local: PathBuf,
    idb: PathBuf,
    cache: PathBuf,
    old_general: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for name in ["Library", "Caches", "profile"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        let selection = TrustedStoreSelection::from_host(
            HostConfiguration {
                library_directory: root.join("Library"),
                cache_directory: root.join("Caches"),
                application_identifier: "fixture.studio".into(),
                inside_app_container: false,
                profile_directory: root.join("profile"),
                store: StoreIdentity::Default,
                framework_build: "22625.2.5.11.1".into(),
            },
            SourceFence {
                source_identity: "a".repeat(64),
                document_token: "private-fixture-document".into(),
            },
        )
        .unwrap();
        let website = root.join("Library/WebKit/fixture.studio/WebsiteData");
        let cache_root = root.join("Caches/fixture.studio/WebKit");
        Self {
            _directory: directory,
            selection,
            general: website.join("Default"),
            local: website.join("LocalStorage"),
            idb: website.join("IndexedDB"),
            cache: cache_root.join("CacheStorage"),
            old_general: cache_root.join("Storage"),
        }
    }
    fn modern(&self, pair: &OriginPair) -> PathBuf {
        self.at_general(&self.general, pair)
    }
    fn at_general(&self, root: &Path, pair: &OriginPair) -> PathBuf {
        write(&root.join("salt"), &FIXTURE_SALT);
        let directory = root.join(hash(&pair.top)).join(hash(&pair.client));
        write(&directory.join("origin"), &encoded(pair));
        fs::create_dir_all(directory.join("IndexedDB")).unwrap();
        directory
    }
}

#[test]
fn native_origin_inventory_accepts_observed_full_metadata_and_hash_contract() {
    let fixture = Fixture::new();
    let pair = root_pair();
    let directory = fixture.modern(&pair);
    // A storage payload is not an origin metadata file and must not be decoded.
    write(
        &directory.join("IndexedDB/opaque.sqlite3"),
        b"not a metadata packet",
    );
    write(&directory.join("FileSystem/opaque.bin"), b"unread");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            directory.join("IndexedDB/opaque.sqlite3"),
            fs::Permissions::from_mode(0),
        )
        .unwrap();
    }
    assert_eq!(encoded(&pair).len(), 50);
    let inventory = inspect(&fixture.selection).unwrap();
    assert_eq!(inventory.format, FORMAT);
    assert_eq!(inventory.origins, vec![pair]);
    assert_eq!(
        inventory.storage[0].kinds,
        BTreeSet::from([StorageKind::IndexedDb, StorageKind::FileSystem])
    );
    inventory
        .require_same_source(&fixture.selection, &fixture.selection.fence)
        .unwrap();
}

#[test]
fn native_origin_inventory_refuses_wrong_hash_and_unconsumed_metadata() {
    let fixture = Fixture::new();
    let pair = root_pair();
    let directory = fixture.modern(&pair);
    let valid = encoded(&pair);
    for end in [0, 1, 4, 5, 10, 25, 49] {
        write(&directory.join("origin"), &valid[..end]);
        assert_eq!(
            inspect(&fixture.selection).err(),
            Some(Held("unsupported_format"))
        );
    }
    let mut invalid_boolean = valid.clone();
    invalid_boolean[4] = 2;
    write(&directory.join("origin"), &invalid_boolean);
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("unsupported_format"))
    );
    let mut null_string = valid.clone();
    null_string[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    write(&directory.join("origin"), &null_string);
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("unsupported_format"))
    );
    let mut bytes = encoded(&pair);
    bytes.push(0);
    write(&directory.join("origin"), &bytes);
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("unsupported_format"))
    );
    write(&directory.join("origin"), &encoded(&pair));
    let moved = directory.parent().unwrap().join("wrong-directory-hash");
    fs::rename(&directory, &moved).unwrap();
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("origin_hash_mismatch"))
    );
}

#[test]
fn native_origin_inventory_preserves_partitioned_origins_and_utf16_ports() {
    let fixture = Fixture::new();
    let mut pair = OriginPair {
        top: origin("tauri", "localhost"),
        client: origin("studio-view", "studio-nrxwgylm.localhost"),
    };
    fixture.modern(&pair);
    let mut unicode = origin("fixture", "host-\u{03bb}.invalid");
    unicode.port = Some(1443);
    let second = OriginPair {
        top: unicode.clone(),
        client: unicode,
    };
    fixture.modern(&second);
    let inventory = inspect(&fixture.selection).unwrap();
    let mut expected = vec![pair.clone(), second];
    expected.sort();
    assert_eq!(inventory.origins, expected);
    std::mem::swap(&mut pair.top, &mut pair.client);
    assert!(!inventory.origins.contains(&pair));
}

#[test]
fn native_origin_inventory_unions_legacy_and_old_general_layouts() {
    let fixture = Fixture::new();
    fixture.modern(&root_pair());
    write(
        &fixture.local.join("tauri_localhost_0.localstorage"),
        b"unread",
    );
    write(
        &fixture
            .idb
            .join("v0/https_first.invalid_443/db/IndexedDB.sqlite3"),
        b"unread",
    );
    write(
        &fixture.idb.join(format!(
            "v1/tauri_localhost_0/studio-view_studio-nrxwgylm.localhost_0/{}/IndexedDB.sqlite3",
            "d".repeat(64),
        )),
        b"unread",
    );
    write(
        &fixture
            .idb
            .join("https_older.invalid_0/db/IndexedDB.sqlite3"),
        b"unread",
    );
    let cache = OriginPair {
        top: origin("fixture", "cache.invalid"),
        client: origin("fixture", "cache.invalid"),
    };
    write(&fixture.cache.join("salt"), &FIXTURE_SALT);
    write(
        &fixture.cache.join(cache_hash(&cache)).join("origin"),
        &encoded(&cache),
    );
    let old = OriginPair {
        top: origin("fixture", "old.invalid"),
        client: origin("fixture", "old.invalid"),
    };
    fixture.at_general(&fixture.old_general, &old);
    let inventory = inspect(&fixture.selection).unwrap();
    let mut first = origin("https", "first.invalid");
    first.port = Some(443);
    let older = origin("https", "older.invalid");
    let mut expected = vec![
        root_pair(),
        OriginPair {
            top: first.clone(),
            client: first,
        },
        OriginPair {
            top: root_pair().top,
            client: origin("studio-view", "studio-nrxwgylm.localhost"),
        },
        OriginPair {
            top: older.clone(),
            client: older,
        },
        cache,
        old,
    ];
    expected.sort();
    assert_eq!(inventory.origins, expected);
}

#[test]
fn native_origin_inventory_refuses_unknown_nonempty_or_missing_origin_entries() {
    let fixture = Fixture::new();
    let directory = fixture.modern(&root_pair());
    write(&directory.join("unknown-store/unread.bin"), b"unread");
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("unsupported_entry"))
    );
    fs::remove_dir_all(directory.join("unknown-store")).unwrap();
    fs::remove_file(directory.join("origin")).unwrap();
    assert_eq!(
        inspect(&fixture.selection).err(),
        Some(Held("missing_origin_metadata"))
    );
}

#[test]
fn native_origin_inventory_refuses_changed_document_and_store_binding() {
    let fixture = Fixture::new();
    fixture.modern(&root_pair());
    let inventory = inspect(&fixture.selection).unwrap();
    let mut changed = fixture.selection.clone();
    changed.fence.document_token = "replacement-document".into();
    assert_eq!(
        inventory.require_same_source(&fixture.selection, &changed.fence),
        Err(Held("source_changed"))
    );
    changed = fixture.selection.clone();
    changed.configuration.store = StoreIdentity::Identified([7; 16]);
    assert_eq!(
        inventory.require_same_source(&changed, &fixture.selection.fence),
        Err(Held("source_changed"))
    );
    fixture.modern(&OriginPair {
        top: origin("tauri", "localhost"),
        client: origin("studio-view", "studio-nrxwgylm.localhost"),
    });
    assert_eq!(
        inventory.require_same_source(&fixture.selection, &fixture.selection.fence),
        Err(Held("source_changed"))
    );
}

#[test]
fn native_origin_inventory_uses_host_custom_store_and_container_roots() {
    let mut fixture = Fixture::new();
    fixture.modern(&root_pair());
    fixture.selection.configuration.store = StoreIdentity::Identified([0x11; 16]);
    let general = fixture.selection.configuration.library_directory.join(
        "WebKit/fixture.studio/WebsiteDataStore/11111111-1111-1111-1111-111111111111/Origins",
    );
    let custom = OriginPair {
        top: origin("fixture", "custom.invalid"),
        client: origin("fixture", "custom.invalid"),
    };
    fixture.at_general(&general, &custom);
    assert_eq!(inspect(&fixture.selection).unwrap().origins, vec![custom]);
    fixture.selection.configuration.inside_app_container = true;
    let container_general = fixture
        .selection
        .configuration
        .library_directory
        .join("WebKit/WebsiteDataStore/11111111-1111-1111-1111-111111111111/Origins");
    fixture.at_general(&container_general, &root_pair());
    assert_eq!(
        inspect(&fixture.selection).unwrap().origins,
        vec![root_pair()]
    );
}

#[cfg(unix)]
#[test]
fn native_origin_inventory_refuses_symlinks_without_reading_the_target() {
    let fixture = Fixture::new();
    let directory = fixture.modern(&root_pair());
    let destination = fixture._directory.path().join("outside-origin");
    write(&destination, &encoded(&root_pair()));
    fs::remove_file(directory.join("origin")).unwrap();
    std::os::unix::fs::symlink(destination, directory.join("origin")).unwrap();
    assert_eq!(inspect(&fixture.selection).err(), Some(Held("symlink")));
    fs::remove_file(directory.join("origin")).unwrap();
    write(&directory.join("origin"), &encoded(&root_pair()));
    std::os::unix::fs::symlink(
        fixture._directory.path().join("outside-origin"),
        directory.join("IndexedDB/opaque-link"),
    )
    .unwrap();
    assert_eq!(inspect(&fixture.selection).err(), Some(Held("symlink")));
}
