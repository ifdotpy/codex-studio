//! Read-only origin metadata for the host-selected WebKit store.
//!
//! The caller holds the native profile lock, closes legacy effect admission,
//! and keeps App mount blocked through the final private-world token check.
//! No type in this module is a renderer request or accepts renderer paths.
use std::{collections::BTreeSet, path::PathBuf};

/// Application format descriptor, not an embedded WebKit version field.
pub(crate) const FORMAT: &str = "webkit-client-origin-native-le-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StoreIdentity {
    Default,
    Identified([u8; 16]),
}

/// Fields come from the actual WKWebView and Foundation host configuration.
/// Nonpersistent stores must be refused before constructing this selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostConfiguration {
    pub(crate) library_directory: PathBuf,
    pub(crate) cache_directory: PathBuf,
    pub(crate) application_identifier: String,
    pub(crate) inside_app_container: bool,
    pub(crate) profile_directory: PathBuf,
    pub(crate) store: StoreIdentity,
    pub(crate) framework_build: String,
}

/// Minted only while the host's legacy-effect and pre-App fences remain held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFence {
    pub(crate) source_identity: String,
    pub(crate) document_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrustedStoreSelection {
    configuration: HostConfiguration,
    fence: SourceFence,
}

impl TrustedStoreSelection {
    pub(crate) fn from_host(
        configuration: HostConfiguration,
        fence: SourceFence,
    ) -> Result<Self, Held> {
        if !configuration.library_directory.is_absolute()
            || !configuration.cache_directory.is_absolute()
            || !configuration.profile_directory.is_absolute()
            || configuration.application_identifier.is_empty()
            || configuration
                .application_identifier
                .split('.')
                .any(str::is_empty)
            || !configuration
                .application_identifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
            || configuration.framework_build.is_empty()
            || fence.source_identity.len() != 64
            || !fence
                .source_identity
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            || fence.document_token.is_empty()
            || fence.document_token.len() > crate::legacy_capture::DOCUMENT_TOKEN_MAX_BYTES
            || matches!(configuration.store, StoreIdentity::Identified(id) if id == [0; 16])
        {
            return Err(Held("invalid_host_binding"));
        }
        Ok(Self {
            configuration,
            fence,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Origin {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OriginPair {
    pub(crate) top: Origin,
    pub(crate) client: Origin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StorageKind {
    LocalStorage,
    IndexedDb,
    CacheStorage,
    FileSystem,
    SessionStorage,
    BackgroundFetchStorage,
    ServiceWorkers,
}

pub(crate) struct OriginStorage {
    pub(crate) origin: OriginPair,
    /// Nonempty storage subtrees, not proof that their payload was exported.
    pub(crate) kinds: BTreeSet<StorageKind>,
}

pub(crate) struct OriginInventory {
    pub(crate) origins: Vec<OriginPair>,
    pub(crate) storage: Vec<OriginStorage>,
    pub(crate) format: &'static str,
    selection: TrustedStoreSelection,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    roots: Vec<(PathBuf, Option<filesystem::Identity>)>,
}

impl OriginInventory {
    /// Call after capture with another host-stamped private-world token.
    pub(crate) fn require_same_source(
        &self,
        selection: &TrustedStoreSelection,
        current_fence: &SourceFence,
    ) -> Result<(), Held> {
        if &self.selection != selection || &self.selection.fence != current_fence {
            return Err(Held("source_changed"));
        }
        // Capture is read-only. A new origin or replaced root is not covered by
        // the earlier inventory, even if the same document remains loaded.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let current = inspect(selection)?;
            if self.roots != current.roots
                || self.origins != current.origins
                || self
                    .storage
                    .iter()
                    .map(|entry| (&entry.origin, &entry.kinds))
                    .ne(current
                        .storage
                        .iter()
                        .map(|entry| (&entry.origin, &entry.kinds)))
            {
                return Err(Held("source_changed"));
            }
        }
        Ok(())
    }
}

/// Stable category only. Never include source paths or origin values in errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held(pub(crate) &'static str);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The saved browser origin inventory remains held ({}).",
            self.0
        )
    }
}

pub(crate) fn inspect(selection: &TrustedStoreSelection) -> Result<OriginInventory, Held> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return filesystem::inspect(selection);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = selection;
        Err(Held("unsupported_platform"))
    }
}

// Format owners, pinned independently of the installed framework version:
// WebKit 1c19ff8379e1fa2a96799943e6d0e6b503f66ffa:
// Source/WebCore/platform/WebCorePersistentCoders.cpp (ClientOrigin),
// Source/WTF/wtf/persistence/PersistentCoders.cpp (String),
// Source/WebKit/NetworkProcess/storage/NetworkStorageManager.cpp (modern roots),
// LocalStorageManager.cpp, IDBStorageManager.cpp and CacheStorageManager.cpp.
// Full consumption and directory hashes identify this observed format. There is
// no embedded WebKit version, and no claim that WebKit main is the shipped code.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod filesystem {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha1::Digest as Sha1Digest;
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeMap,
        ffi::{CStr, CString, OsStr, OsString},
        fs::File,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::{Component, Path},
    };

    // Bytes in WTF::FileSystem::Salt, not a credential or an authorization token.
    const SALT_BYTES: usize = 8;
    // Limits apply to metadata only. Stored database and Blob bytes stay unread.
    const MAX_ORIGIN_BYTES: usize = 16 * 1024;
    const MAX_ENTRIES: usize = 100_000;
    const MAX_DEPTH: usize = 64;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Identity(u64, u64);

    struct Directory(File);
    enum Kind {
        File(u64),
        Directory,
    }

    fn error(error: std::io::Error) -> Held {
        if error.raw_os_error() == Some(libc::ELOOP) {
            Held("symlink")
        } else {
            Held("metadata_io")
        }
    }
    fn name(value: &OsStr) -> Result<CString, Held> {
        CString::new(value.as_bytes()).map_err(|_| Held("unsupported_entry"))
    }
    fn owned(file: &File) -> Result<std::fs::Metadata, Held> {
        let metadata = file.metadata().map_err(error)?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
            return Err(Held("foreign_metadata_owner"));
        }
        Ok(metadata)
    }
    impl Directory {
        fn absolute(path: &Path) -> Result<Option<Self>, Held> {
            if !path.is_absolute()
                || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
            {
                return Err(Held("invalid_host_binding"));
            }
            let root = c"/";
            let fd = unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            let mut current = Self(unsafe { File::from_raw_fd(fd) });
            for component in path.components() {
                match component {
                    Component::RootDir => (),
                    Component::Normal(value) => {
                        let Some(child) = current.child(value)? else {
                            return Ok(None);
                        };
                        current = child;
                    }
                    _ => return Err(Held("invalid_host_binding")),
                }
            }
            owned(&current.0)?;
            Ok(Some(current))
        }
        fn child(&self, value: &OsStr) -> Result<Option<Self>, Held> {
            let value_c = name(value)?;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    value_c.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd < 0 {
                let failure = std::io::Error::last_os_error();
                if failure.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
                // Some systems return ENOTDIR rather than ELOOP for a link.
                self.kind(value)?;
                return Err(error(failure));
            }
            Ok(Some(Self(unsafe { File::from_raw_fd(fd) })))
        }
        fn identity(&self) -> Result<Identity, Held> {
            let metadata = owned(&self.0)?;
            Ok(Identity(metadata.dev(), metadata.ino()))
        }
        fn kind(&self, value: &OsStr) -> Result<Kind, Held> {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            let value = name(value)?;
            if unsafe {
                libc::fstatat(
                    self.0.as_raw_fd(),
                    value.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(error(std::io::Error::last_os_error()));
            }
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_mode & libc::S_IFMT == libc::S_IFLNK {
                return Err(Held("symlink"));
            }
            if metadata.st_uid != unsafe { libc::geteuid() } || metadata.st_mode & 0o022 != 0 {
                return Err(Held("foreign_metadata_owner"));
            }
            match metadata.st_mode & libc::S_IFMT {
                libc::S_IFDIR => Ok(Kind::Directory),
                libc::S_IFREG if metadata.st_size >= 0 => Ok(Kind::File(metadata.st_size as u64)),
                _ => Err(Held("unsupported_entry")),
            }
        }
        fn entries(&self, budget: &mut usize) -> Result<Vec<OsString>, Held> {
            owned(&self.0)?;
            // A fresh description avoids sharing a directory cursor with the
            // retained root descriptor or another metadata pass.
            let dot = c".";
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            let pointer = unsafe { libc::fdopendir(fd) };
            if pointer.is_null() {
                let failure = std::io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                }
                return Err(error(failure));
            }
            struct Listing(*mut libc::DIR);
            impl Drop for Listing {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let listing = Listing(pointer);
            let mut result = vec![];
            loop {
                #[cfg(target_os = "macos")]
                unsafe {
                    *libc::__error() = 0;
                }
                #[cfg(target_os = "linux")]
                unsafe {
                    *libc::__errno_location() = 0;
                }
                let entry = unsafe { libc::readdir(listing.0) };
                if entry.is_null() {
                    let failure = std::io::Error::last_os_error();
                    if failure.raw_os_error() != Some(0) {
                        return Err(error(failure));
                    }
                    break;
                }
                let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                *budget = budget.checked_sub(1).ok_or(Held("metadata_bound"))?;
                result.push(OsStr::from_bytes(bytes).to_owned());
            }
            result.sort();
            Ok(result)
        }
        fn read(&self, value: &OsStr, maximum: usize) -> Result<Vec<u8>, Held> {
            let Kind::File(size) = self.kind(value)? else {
                return Err(Held("unsupported_format"));
            };
            if size > maximum as u64 {
                return Err(Held("metadata_bound"));
            }
            let value = name(value)?;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    value.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let before = owned(&file)?;
            if !before.is_file() || before.len() > maximum as u64 {
                return Err(Held("metadata_bound"));
            }
            let mut bytes = Vec::new();
            (&file)
                .take(maximum as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(error)?;
            let after = owned(&file)?;
            if bytes.len() as u64 != before.len()
                || (
                    before.dev(),
                    before.ino(),
                    before.len(),
                    before.mtime(),
                    before.mtime_nsec(),
                ) != (
                    after.dev(),
                    after.ino(),
                    after.len(),
                    after.mtime(),
                    after.mtime_nsec(),
                )
            {
                return Err(Held("metadata_changed"));
            }
            Ok(bytes)
        }
    }

    struct Decoder<'a> {
        bytes: &'a [u8],
        offset: usize,
    }
    impl<'a> Decoder<'a> {
        fn take(&mut self, count: usize) -> Result<&'a [u8], Held> {
            let end = self
                .offset
                .checked_add(count)
                .ok_or(Held("unsupported_format"))?;
            let value = self
                .bytes
                .get(self.offset..end)
                .ok_or(Held("unsupported_format"))?;
            self.offset = end;
            Ok(value)
        }
        fn boolean(&mut self) -> Result<bool, Held> {
            match self.take(1)?[0] {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(Held("unsupported_format")),
            }
        }
        fn string(&mut self) -> Result<String, Held> {
            let length = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
            if length > MAX_ORIGIN_BYTES {
                return Err(Held("unsupported_format"));
            }
            let latin1 = self.boolean()?;
            let bytes = self.take(
                length
                    .checked_mul(if latin1 { 1 } else { 2 })
                    .ok_or(Held("unsupported_format"))?,
            )?;
            let text: String = if latin1 {
                bytes.iter().map(|byte| char::from(*byte)).collect()
            } else {
                let units: Vec<_> = bytes
                    .chunks_exact(2)
                    .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                    .collect();
                String::from_utf16(&units).map_err(|_| Held("unsupported_format"))?
            };
            if text.contains('\0') {
                return Err(Held("unsupported_format"));
            }
            Ok(text)
        }
        fn origin(&mut self) -> Result<Origin, Held> {
            let scheme = self.string()?;
            let host = self.string()?;
            let port = if self.boolean()? {
                Some(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
            } else {
                None
            };
            if scheme.is_empty()
                || !scheme.bytes().enumerate().all(|(index, byte)| {
                    if index == 0 {
                        byte.is_ascii_lowercase()
                    } else {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(&byte)
                    }
                })
                || (host.is_empty() && scheme != "file")
            {
                return Err(Held("unsupported_format"));
            }
            Ok(Origin { scheme, host, port })
        }
    }
    fn decode(bytes: &[u8]) -> Result<OriginPair, Held> {
        let mut decoder = Decoder { bytes, offset: 0 };
        let pair = OriginPair {
            top: decoder.origin()?,
            client: decoder.origin()?,
        };
        if decoder.offset != bytes.len() {
            return Err(Held("unsupported_format"));
        }
        Ok(pair)
    }
    fn text(origin: &Origin) -> String {
        if origin.scheme == "file" {
            return "file://".into();
        }
        format!(
            "{}://{}{}",
            origin.scheme,
            origin.host,
            origin
                .port
                .map(|port| format!(":{port}"))
                .unwrap_or_default()
        )
    }
    fn modern_hash(origin: &Origin, salt: &[u8]) -> String {
        let mut hash = Sha256::new();
        hash.update(text(origin).as_bytes());
        hash.update(salt);
        URL_SAFE_NO_PAD.encode(hash.finalize())
    }
    fn cache_hash(pair: &OriginPair, salt: &[u8]) -> String {
        // Legacy NetworkCache::Key owner: SHA1(salt || top || client).
        // This is an existing directory identifier, not new authentication.
        let mut hash = sha1::Sha1::new();
        hash.update(salt);
        hash.update(text(&pair.top).as_bytes());
        hash.update(text(&pair.client).as_bytes());
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }
    fn identifier(value: &str) -> Result<Origin, Held> {
        let first = value.find('_').ok_or(Held("unsupported_entry"))?;
        let last = value
            .rfind('_')
            .filter(|last| *last != first)
            .ok_or(Held("unsupported_entry"))?;
        let scheme = &value[..first];
        let host = &value[first + 1..last];
        let port = value[last + 1..]
            .parse::<u16>()
            .map_err(|_| Held("unsupported_entry"))?;
        // The observed app identifiers are ASCII and need no filename escaping.
        // Unknown legacy filename encodings remain held, never URL-decoded.
        if !scheme.bytes().enumerate().all(|(i, b)| {
            if i == 0 {
                b.is_ascii_lowercase()
            } else {
                b.is_ascii_lowercase() || b.is_ascii_digit() || b"+.-".contains(&b)
            }
        }) || scheme.is_empty()
            || !host.is_ascii()
            || host
                .bytes()
                .any(|b| !b.is_ascii_alphanumeric() && !b".-_[]:".contains(&b))
            || (host.is_empty() && scheme != "file")
            || port.to_string() != value[last + 1..]
        {
            return Err(Held("unsupported_entry"));
        }
        Ok(Origin {
            scheme: scheme.into(),
            host: host.into(),
            port: (port != 0).then_some(port),
        })
    }

    struct Scan {
        budget: usize,
        origins: BTreeMap<OriginPair, BTreeSet<StorageKind>>,
        roots: Vec<(PathBuf, Option<Identity>)>,
    }
    impl Scan {
        fn root(&mut self, path: &Path) -> Result<Option<Directory>, Held> {
            let root = Directory::absolute(path)?;
            let identity = root.as_ref().map(Directory::identity).transpose()?;
            self.roots.push((path.into(), identity));
            Ok(root)
        }
        fn add(&mut self, pair: OriginPair, kind: Option<StorageKind>) {
            let entry = self.origins.entry(pair).or_default();
            entry.extend(kind);
        }
        fn entries(&mut self, dir: &Directory) -> Result<Vec<OsString>, Held> {
            dir.entries(&mut self.budget)
        }
        fn finder_metadata(dir: &Directory, entry: &OsStr) -> Result<bool, Held> {
            if entry != ".DS_Store" {
                return Ok(false);
            }
            if !matches!(dir.kind(entry)?, Kind::File(_)) {
                return Err(Held("unsupported_entry"));
            }
            Ok(true)
        }
        fn layout(&mut self, path: &Path, allowed: &[&str]) -> Result<(), Held> {
            let Some(root) = self.root(path)? else {
                return Ok(());
            };
            for entry in self.entries(&root)? {
                let kind = root.kind(&entry)?;
                if Self::finder_metadata(&root, &entry)? {
                    continue;
                }
                if entry.to_str().is_some_and(|name| allowed.contains(&name)) {
                    continue;
                }
                // An empty old directory contains no origin or payload. Other
                // unknown entries cannot be silently excluded from coverage.
                if matches!(kind, Kind::Directory)
                    && !self.audit(&root.child(&entry)?.ok_or(Held("metadata_changed"))?, 0)?
                {
                    continue;
                }
                return Err(Held("unsupported_entry"));
            }
            Ok(())
        }
        fn audit(&mut self, dir: &Directory, depth: usize) -> Result<bool, Held> {
            if depth > MAX_DEPTH {
                return Err(Held("metadata_bound"));
            }
            let entries = self.entries(dir)?;
            for entry in &entries {
                if matches!(dir.kind(entry)?, Kind::Directory) {
                    let child = dir.child(entry)?.ok_or(Held("metadata_changed"))?;
                    self.audit(&child, depth + 1)?;
                }
            }
            Ok(!entries.is_empty())
        }
        fn salt(&self, dir: &Directory) -> Result<Vec<u8>, Held> {
            let bytes = dir.read(OsStr::new("salt"), SALT_BYTES)?;
            if bytes.len() != SALT_BYTES {
                return Err(Held("unsupported_format"));
            }
            Ok(bytes)
        }
        fn metadata(&mut self, dir: &Directory) -> Result<OriginPair, Held> {
            if !self.entries(dir)?.iter().any(|entry| entry == "origin") {
                return Err(Held("missing_origin_metadata"));
            }
            decode(&dir.read(OsStr::new("origin"), MAX_ORIGIN_BYTES)?)
        }
        fn modern(&mut self, path: &Path) -> Result<(), Held> {
            let Some(root) = self.root(path)? else {
                return Ok(());
            };
            let entries = self.entries(&root)?;
            if entries.is_empty() {
                return Ok(());
            }
            let salt = self.salt(&root)?;
            for top in entries {
                if Self::finder_metadata(&root, &top)? {
                    continue;
                }
                if top == "salt" {
                    root.kind(&top)?;
                    continue;
                }
                if !matches!(root.kind(&top)?, Kind::Directory) {
                    return Err(Held("unsupported_entry"));
                }
                let top_dir = root.child(&top)?.ok_or(Held("metadata_changed"))?;
                for client in self.entries(&top_dir)? {
                    if Self::finder_metadata(&top_dir, &client)? {
                        continue;
                    }
                    if !matches!(top_dir.kind(&client)?, Kind::Directory) {
                        return Err(Held("unsupported_entry"));
                    }
                    let dir = top_dir.child(&client)?.ok_or(Held("metadata_changed"))?;
                    if self.entries(&dir)?.is_empty() {
                        continue;
                    }
                    let pair = self.metadata(&dir)?;
                    if top != OsStr::new(&modern_hash(&pair.top, &salt))
                        || client != OsStr::new(&modern_hash(&pair.client, &salt))
                    {
                        return Err(Held("origin_hash_mismatch"));
                    }
                    self.add(pair.clone(), None);
                    for entry in self.entries(&dir)? {
                        let kind = dir.kind(&entry)?;
                        if entry == "origin" || entry == "persisted" || entry == ".DS_Store" {
                            if !matches!(kind, Kind::File(_)) {
                                return Err(Held("unsupported_entry"));
                            }
                            continue;
                        }
                        let storage = match entry.to_str() {
                            Some("LocalStorage") => Some(StorageKind::LocalStorage),
                            Some("IndexedDB") => Some(StorageKind::IndexedDb),
                            Some("CacheStorage") => Some(StorageKind::CacheStorage),
                            Some("FileSystem") => Some(StorageKind::FileSystem),
                            Some("SessionStorage") => Some(StorageKind::SessionStorage),
                            Some("BackgroundFetchStorage") => {
                                Some(StorageKind::BackgroundFetchStorage)
                            }
                            Some("ServiceWorkers") => Some(StorageKind::ServiceWorkers),
                            _ => None,
                        };
                        if !matches!(kind, Kind::Directory) {
                            return Err(Held("unsupported_entry"));
                        }
                        let child = dir.child(&entry)?.ok_or(Held("metadata_changed"))?;
                        if self.audit(&child, 0)? {
                            self.add(
                                pair.clone(),
                                Some(storage.ok_or(Held("unsupported_entry"))?),
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        fn local(&mut self, path: &Path) -> Result<(), Held> {
            let Some(root) = self.root(path)? else {
                return Ok(());
            };
            let entries = self.entries(&root)?;
            for entry in &entries {
                if Self::finder_metadata(&root, entry)? {
                    continue;
                }
                if !matches!(root.kind(entry)?, Kind::File(_)) {
                    return Err(Held("unsupported_entry"));
                }
                let value = entry.to_str().ok_or(Held("unsupported_entry"))?;
                let base = ["-wal", "-shm", "-journal"]
                    .iter()
                    .find_map(|suffix| value.strip_suffix(suffix))
                    .unwrap_or(value);
                if base != value && !entries.iter().any(|entry| entry == OsStr::new(base)) {
                    return Err(Held("unsupported_entry"));
                }
                let origin = identifier(
                    base.strip_suffix(".localstorage")
                        .ok_or(Held("unsupported_entry"))?,
                )?;
                self.add(
                    OriginPair {
                        top: origin.clone(),
                        client: origin,
                    },
                    Some(StorageKind::LocalStorage),
                );
            }
            Ok(())
        }
        fn idb_database(
            &mut self,
            root: &Directory,
            name: &OsStr,
            hashed: bool,
        ) -> Result<(), Held> {
            // SQLiteFileSystem::computeHashForFileName owns the v1 SHA256 name.
            // The database name itself remains inside the unread SQLite file.
            if hashed
                && !name.to_str().is_some_and(|name| {
                    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err(Held("unsupported_entry"));
            }
            if !matches!(root.kind(OsStr::new("IndexedDB.sqlite3"))?, Kind::File(_)) {
                return Err(Held("unsupported_entry"));
            }
            self.audit(root, 0)?;
            Ok(())
        }
        fn idb_origin(&mut self, root: &Directory, top: Origin, hashed: bool) -> Result<(), Held> {
            for entry in self.entries(root)? {
                if Self::finder_metadata(root, &entry)? {
                    continue;
                }
                if !matches!(root.kind(&entry)?, Kind::Directory) {
                    return Err(Held("unsupported_entry"));
                }
                let child = root.child(&entry)?.ok_or(Held("metadata_changed"))?;
                let children = self.entries(&child)?;
                if children.is_empty() {
                    continue;
                }
                // The SQLite filename identifies a first-party database directory.
                // Its contents remain opaque; the fixed WebKit reader owns records.
                if children.iter().any(|name| name == "IndexedDB.sqlite3") {
                    self.idb_database(&child, &entry, hashed)?;
                    self.add(
                        OriginPair {
                            top: top.clone(),
                            client: top.clone(),
                        },
                        Some(StorageKind::IndexedDb),
                    );
                } else {
                    let client = identifier(entry.to_str().ok_or(Held("unsupported_entry"))?)?;
                    for database in children {
                        if Self::finder_metadata(&child, &database)? {
                            continue;
                        }
                        if !matches!(child.kind(&database)?, Kind::Directory) {
                            return Err(Held("unsupported_entry"));
                        }
                        let directory = child.child(&database)?.ok_or(Held("metadata_changed"))?;
                        let names = self.entries(&directory)?;
                        if names.is_empty() {
                            continue;
                        }
                        if !names.iter().any(|name| name == "IndexedDB.sqlite3") {
                            return Err(Held("unsupported_entry"));
                        }
                        self.idb_database(&directory, &database, hashed)?;
                        self.add(
                            OriginPair {
                                top: top.clone(),
                                client: client.clone(),
                            },
                            Some(StorageKind::IndexedDb),
                        );
                    }
                }
            }
            Ok(())
        }
        fn idb_version(&mut self, root: &Directory, hashed: bool) -> Result<(), Held> {
            for entry in self.entries(root)? {
                if Self::finder_metadata(root, &entry)? {
                    continue;
                }
                let top = identifier(entry.to_str().ok_or(Held("unsupported_entry"))?)?;
                if !matches!(root.kind(&entry)?, Kind::Directory) {
                    return Err(Held("unsupported_entry"));
                }
                let dir = root.child(&entry)?.ok_or(Held("metadata_changed"))?;
                self.idb_origin(&dir, top, hashed)?;
            }
            Ok(())
        }
        fn idb(&mut self, path: &Path) -> Result<(), Held> {
            let Some(root) = self.root(path)? else {
                return Ok(());
            };
            for entry in self.entries(&root)? {
                if Self::finder_metadata(&root, &entry)? {
                    continue;
                }
                if !matches!(root.kind(&entry)?, Kind::Directory) {
                    return Err(Held("unsupported_entry"));
                }
                let child = root.child(&entry)?.ok_or(Held("metadata_changed"))?;
                if entry == "v0" || entry == "v1" {
                    self.idb_version(&child, entry == "v1")?;
                } else {
                    self.idb_origin(
                        &child,
                        identifier(entry.to_str().ok_or(Held("unsupported_entry"))?)?,
                        false,
                    )?;
                }
            }
            Ok(())
        }
        fn cache(&mut self, path: &Path) -> Result<(), Held> {
            let Some(root) = self.root(path)? else {
                return Ok(());
            };
            let entries = self.entries(&root)?;
            if entries.is_empty() {
                return Ok(());
            }
            let salt = self.salt(&root)?;
            for entry in entries {
                if Self::finder_metadata(&root, &entry)? {
                    continue;
                }
                if entry == "salt" {
                    root.kind(&entry)?;
                    continue;
                }
                if !matches!(root.kind(&entry)?, Kind::Directory) {
                    return Err(Held("unsupported_entry"));
                }
                let dir = root.child(&entry)?.ok_or(Held("metadata_changed"))?;
                let children = self.entries(&dir)?;
                if children.is_empty() {
                    continue;
                }
                let pair = self.metadata(&dir)?;
                if entry != OsStr::new(&cache_hash(&pair, &salt)) {
                    return Err(Held("origin_hash_mismatch"));
                }
                self.add(pair.clone(), None);
                for child in children {
                    if Self::finder_metadata(&dir, &child)? {
                        continue;
                    }
                    if child == "origin" {
                        dir.kind(&child)?;
                        continue;
                    }
                    if child == "salt" {
                        if self.salt(&dir)? != salt {
                            return Err(Held("origin_hash_mismatch"));
                        }
                        continue;
                    }
                    match dir.kind(&child)? {
                        Kind::Directory => {
                            self.audit(&dir.child(&child)?.ok_or(Held("metadata_changed"))?, 0)?;
                        }
                        Kind::File(_) => (),
                    }
                    self.add(pair.clone(), Some(StorageKind::CacheStorage));
                }
            }
            Ok(())
        }
        fn unsupported(&mut self, path: &Path) -> Result<(), Held> {
            if let Some(root) = self.root(path)?
                && self.audit(&root, 0)?
            {
                return Err(Held("unsupported_storage"));
            }
            Ok(())
        }
    }

    pub(super) fn inspect(selection: &TrustedStoreSelection) -> Result<OriginInventory, Held> {
        if !cfg!(target_endian = "little") {
            return Err(Held("unsupported_format"));
        }
        let config = &selection.configuration;
        let mut scan = Scan {
            budget: MAX_ENTRIES,
            origins: BTreeMap::new(),
            roots: vec![],
        };
        if scan.root(&config.profile_directory)?.is_none() {
            return Err(Held("invalid_host_binding"));
        }
        let namespace = |base: &Path, library: bool| {
            let mut path = base.to_path_buf();
            if library {
                path.push("WebKit");
            }
            if !config.inside_app_container {
                path.push(&config.application_identifier);
            }
            if !library {
                path.push("WebKit");
            }
            path
        };
        let library = namespace(&config.library_directory, true);
        let caches = namespace(&config.cache_directory, false);
        let (website, general, cache, old_general) = match config.store {
            StoreIdentity::Default => {
                let website = library.join("WebsiteData");
                (
                    website.clone(),
                    website.join("Default"),
                    caches.join("CacheStorage"),
                    Some(caches.join("Storage")),
                )
            }
            StoreIdentity::Identified(identifier) => {
                let hex: String = identifier
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let uuid = format!(
                    "{}-{}-{}-{}-{}",
                    &hex[..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..]
                );
                let website = library.join("WebsiteDataStore").join(uuid);
                (
                    website.clone(),
                    website.join("Origins"),
                    website.join("CacheStorage"),
                    None,
                )
            }
        };
        // These names come from WebsiteDataStoreCocoa's directory owners.
        // Security metadata and credentials are classified, never opened or
        // claimed as migrated application payloads by this origin inventory.
        let mut website_layout = vec![
            "LocalStorage",
            "IndexedDB",
            "WebSQL",
            "MediaKeys",
            "SearchHistory",
            "DeviceIdHashSalts",
            "MediaKeysHashSalts",
            "ResourceLoadStatistics",
            "ResourceMonitorThrottler",
            "EnhancedSecurity",
            "IsolatedSites",
        ];
        match config.store {
            StoreIdentity::Default => {
                website_layout.push("Default");
                scan.layout(
                    &caches,
                    &[
                        "CacheStorage",
                        "Storage",
                        "ServiceWorkers",
                        "NetworkCache",
                        "HSTS",
                        "AlternativeServices",
                    ],
                )?;
            }
            StoreIdentity::Identified(_) => website_layout.extend([
                "Origins",
                "CacheStorage",
                "ServiceWorkers",
                "NetworkCache",
                "HSTS",
                "AlternativeServices",
                "Cookies",
                "MediaCache",
            ]),
        }
        scan.layout(&website, &website_layout)?;
        scan.modern(&general)?;
        if let Some(old_general) = old_general {
            scan.modern(&old_general)?;
        }
        scan.local(&website.join("LocalStorage"))?;
        scan.idb(&website.join("IndexedDB"))?;
        scan.cache(&cache)?;
        for unsupported in ["WebSQL", "MediaKeys", "SearchHistory"] {
            scan.unsupported(&website.join(unsupported))?;
        }
        scan.unsupported(&match config.store {
            StoreIdentity::Default => caches.join("ServiceWorkers"),
            _ => website.join("ServiceWorkers"),
        })?;
        let storage: Vec<_> = scan
            .origins
            .into_iter()
            .map(|(origin, kinds)| OriginStorage { origin, kinds })
            .collect();
        Ok(OriginInventory {
            origins: storage.iter().map(|entry| entry.origin.clone()).collect(),
            storage,
            format: FORMAT,
            selection: selection.clone(),
            roots: scan.roots,
        })
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "native_origin_inventory_tests.rs"]
mod tests;
