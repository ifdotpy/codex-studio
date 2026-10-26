//! Local network (Bonjour) advertisement of a Studio server. This file is the
//! owner. The server advertises with it (`runtime-server/src/bonjour.rs`).
//! The iOS shell includes this file as source (`native/src-tauri/src/ios_server.rs`),
//! and a native test checks that `Info.ios.plist` lists `SERVICE_TYPE`.
//! Keep this file free of crate imports so that the shell can include it.

/// DNS-SD service type, without the domain.
pub const SERVICE_TYPE: &str = "_layr-studio._tcp";
/// Version of the TXT record layout below.
pub const TXT_VERSION: &str = "1";
/// TXT key of the layout version.
pub const TXT_VERSION_KEY: &str = "txtvers";
/// TXT key of the client URL: the HTTPS origin of the server with a `/` path.
pub const TXT_URL_KEY: &str = "url";
/// Host suffix of every advertised URL. The server advertises only its
/// remote access origin, which `validate_federation_origin` limits to a
/// tailnet host. The shell refuses other hosts, so another device on the
/// network can point it only at a tailnet host, not at any site.
pub const TXT_URL_HOST_SUFFIX: &str = ".ts.net";
/// TXT key of the server name that people see.
pub const TXT_NAME_KEY: &str = "name";
/// TXT key of the API schema hash of the server.
pub const TXT_SCHEMA_KEY: &str = "schema";
/// Key in the remote access file (`REMOTE_ACCESS_FILE`). `false` stops the
/// advertisement at the next server start.
pub const REMOTE_ACCESS_BONJOUR_KEY: &str = "bonjour";
/// Maximum UTF-8 bytes of a DNS-SD service instance name.
pub const INSTANCE_NAME_MAX_BYTES: usize = 63;
/// Maximum bytes of one TXT string, `key=value`.
pub const TXT_ENTRY_MAX_BYTES: usize = 255;
