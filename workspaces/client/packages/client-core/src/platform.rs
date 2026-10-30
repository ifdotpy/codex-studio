//! Pure platform services used by the presentation adapters.
use crate::protocol::{Request, Response};

// SAFETY: The random-byte FFI call writes only into its owned byte array.
#[allow(unsafe_code)]
pub(crate) fn uuid() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex = format!("{:032x}", u128::from_be_bytes(bytes));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn dispatch(request: &Request) -> Response {
    match uuid() {
        Ok(value) => Response::success(request.id.clone(), serde_json::Value::String(value)),
        Err(error) => Response::failure(request.id.clone(), "platform_error", error),
    }
}

/// Hidden Attar checks must not show system prompts, notifications or sound.
pub fn hidden_check() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::args().any(|arg| arg == "--attar-check-scenario"))
}

/// Separate explicit desktop profiles in WebKit as well as SQLite. The host
/// must reject unsupported systems: WebKit otherwise silently uses its default.
pub fn webkit_profile_identifier(directory: &std::path::Path) -> Result<[u8; 16], String> {
    #[cfg(target_os = "macos")]
    if objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
        < 14
    {
        return Err("Separate persistent desktop profiles require macOS 14 or newer. Use the default profile or an ephemeral test profile.".into());
    }
    use sha2::{Digest, Sha256};
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    hash.update(b"codex-studio-webkit-profile-v1\0");
    hash.update(directory.as_os_str().as_encoded_bytes());
    let digest = hash.finalize();
    let mut identifier = [0; 16];
    identifier.copy_from_slice(&digest[..16]);
    identifier[6] = (identifier[6] & 0x0f) | 0x50;
    identifier[8] = (identifier[8] & 0x3f) | 0x80;
    Ok(identifier)
}

#[cfg(test)]
mod webkit_profile_tests {
    use super::*;
    #[test]
    fn explicit_profiles_have_stable_separate_canonical_webkit_stores() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let id = webkit_profile_identifier(first.path()).unwrap();
        assert_eq!(
            id,
            webkit_profile_identifier(&first.path().join(".")).unwrap()
        );
        assert_ne!(id, webkit_profile_identifier(second.path()).unwrap());
        assert!(webkit_profile_identifier(&first.path().join("missing")).is_err());
        #[cfg(unix)]
        {
            let link = second.path().join("alias");
            std::os::unix::fs::symlink(first.path(), &link).unwrap();
            assert_eq!(id, webkit_profile_identifier(&link).unwrap());
        }
    }
}
