//! Where the Mac keeps vault identities between runs: the login Keychain, one generic password
//! per vault (service `layr-desk`, account `<gateway host>/<project>`).

use anyhow::Result;

pub trait KeyStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, identity: &str) -> Result<()>;
    fn remove(&self, account: &str) -> Result<()>;
}

pub fn account(site: &str, project: &str) -> Result<String> {
    let url = reqwest::Url::parse(site)?;
    Ok(format!("{}/{project}", url.host_str().unwrap_or("")))
}

/// The login Keychain through the `security` tool. The tool owns the items, so a new build of
/// layr-desk reads them without a Keychain prompt. Like any Keychain item that a tool can read
/// without a prompt, other programs of the same user can read it too; the Keychain keeps the
/// keys from other users and from a copied disk.
#[cfg(target_os = "macos")]
pub struct Keychain;

#[cfg(target_os = "macos")]
const SERVICE: &str = "layr-desk";

#[cfg(target_os = "macos")]
fn security(args: &[&str]) -> Result<std::process::Output> {
    Ok(std::process::Command::new("/usr/bin/security")
        .args(args)
        .output()?)
}

#[cfg(target_os = "macos")]
impl KeyStore for Keychain {
    fn get(&self, account: &str) -> Result<Option<String>> {
        let out = security(&["find-generic-password", "-s", SERVICE, "-a", account, "-w"])?;
        // Exit 44: the item is not found.
        match out.status.code() {
            Some(0) => Ok(Some(String::from_utf8(out.stdout)?.trim().to_string())),
            Some(44) => Ok(None),
            _ => anyhow::bail!("security: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
    }

    fn set(&self, account: &str, identity: &str) -> Result<()> {
        let out = security(&[
            "add-generic-password",
            "-U",
            "-s",
            SERVICE,
            "-a",
            account,
            "-w",
            identity,
        ])?;
        if !out.status.success() {
            anyhow::bail!("security: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn remove(&self, account: &str) -> Result<()> {
        let out = security(&["delete-generic-password", "-s", SERVICE, "-a", account])?;
        match out.status.code() {
            Some(0) | Some(44) => Ok(()),
            _ => anyhow::bail!("security: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
    }
}

/// The key store of this platform; only macOS has one now.
pub fn system() -> Result<Box<dyn KeyStore>> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(Keychain));
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("keeping vault keys needs the macOS Keychain");
}

/// Keys in memory, for tests.
#[cfg(test)]
#[derive(Default)]
pub struct Memory(pub std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl KeyStore for Memory {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap().get(account).cloned())
    }
    fn set(&self, account: &str, identity: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .insert(account.into(), identity.into());
        Ok(())
    }
    fn remove(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}
