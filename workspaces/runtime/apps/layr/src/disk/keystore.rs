//! Store the decrypted store identity in the user's login Keychain.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

#[cfg(all(test, target_os = "macos"))]
pub(super) fn test_service() {
    std::env::set_var("LAYR_TEST_KEYCHAIN_SERVICE", format!("ws.layr.mount.test.{}", std::process::id()));
}

pub fn account(url: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(url.as_bytes()))
}

#[cfg(target_os = "macos")]
fn service() -> String {
    #[cfg(test)]
    if let Ok(service) = std::env::var("LAYR_TEST_KEYCHAIN_SERVICE") {
        assert!(service.starts_with("ws.layr.mount.test."));
        return service;
    }
    "ws.layr.mount".into()
}

pub fn get(account: &str) -> Result<Option<String>> {
    #[cfg(target_os = "macos")]
    {
        use security_framework::passwords::{generic_password, PasswordOptions};
        return match generic_password(PasswordOptions::new_generic_password(&service(), account)) {
            Ok(bytes) => Ok(Some(String::from_utf8(bytes).context("invalid disk key in Keychain")?)),
            Err(e) if e.code() == -25300 => Ok(None),
            Err(_) => bail!("cannot read the disk key from the login Keychain"),
        };
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = account;
        Ok(None)
    }
}

pub fn set(account: &str, identity: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        security_framework::passwords::set_generic_password(&service(), account, identity.as_bytes())
            .map_err(|_| anyhow::anyhow!("cannot save the disk key in the login Keychain"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (account, identity);
    }
    Ok(())
}

pub fn remove(account: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    match security_framework::passwords::delete_generic_password(&service(), account) {
        Ok(()) => {}
        Err(e) if e.code() == -25300 => {}
        Err(_) => bail!("cannot remove the disk key from the login Keychain"),
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = account;
        bail!("the Keychain is macOS only");
    }
    #[cfg(target_os = "macos")]
    Ok(())
}

pub fn paper_file(path: &Path) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("cannot open the paper key file")?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.mode() & 0o777 != 0o600 || meta.uid() != rustix::process::getuid().as_raw() {
        bail!("the paper key file must belong to this user and have mode 0600");
    }
    bounded(file)
}

pub fn bounded(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 || bytes.is_empty() {
        bail!("invalid paper key input");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid paper key input"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn login_keychain_roundtrip() {
        test_service();
        let account = format!("layr-mount-test-{}", crate::ids::new_id());
        let value = format!("test-marker-{}", crate::ids::new_id());
        set(&account, &value).unwrap();
        let found = get(&account).unwrap();
        let removed = remove(&account);
        assert!(removed.is_ok(), "cannot remove the test Keychain item");
        assert!(get(&account).unwrap().is_none(), "the key remains after lock");
        remove(&account).unwrap();
        assert!(found.as_deref() == Some(value.as_str()), "Keychain roundtrip differs");
    }
    #[test]
    fn private_key_input_refuses_links_and_public_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("layr-disk-key-{}", crate::ids::new_id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, "test words").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(paper_file(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(paper_file(&path).is_ok());
        std::os::unix::fs::symlink(&path, dir.join("link")).unwrap();
        assert!(paper_file(&dir.join("link")).is_err());
        assert!(bounded(&vec![b'a'; 4097][..]).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
