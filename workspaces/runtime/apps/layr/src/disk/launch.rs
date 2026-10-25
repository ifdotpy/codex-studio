//! Start the disk at login and mount it with the macOS WebDAV client.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const LABEL: &str = "ws.layr.mount";

pub(super) fn plist(exe: &Path, url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> String {
    let args = [
        exe.display().to_string(),
        "mount".into(),
        url.into(),
        "--keychain-only".into(),
        "--listen".into(),
        listen.into(),
        "--cache".into(),
        cache.display().to_string(),
        "--cache-size".into(),
        limit.to_string(),
        "--at".into(),
        home.join("Layr").display().to_string(),
    ];
    let args = args.iter().map(|arg| format!("<string>{}</string>", super::webdav::xml(arg))).collect::<String>();
    let log = super::webdav::xml(&home.join("Library/Logs/layr-mount.log").display().to_string());
    let mut environment = format!("<key>HOME</key><string>{}</string>", super::webdav::xml(&home.display().to_string()));
    for name in ["LAYR_ROOT", "XDG_CONFIG_HOME", "LAYR_PERSON_DIR", "LAYR_PERSON_KEY_FILE", "LAYR_PERSON_LOGIN"] {
        if let Some(value) = std::env::var_os(name) {
            environment.push_str(&format!("<key>{name}</key><string>{}</string>", super::webdav::xml(&value.to_string_lossy())));
        }
    }
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array>{args}</array><key>EnvironmentVariables</key><dict>{environment}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{log}</string><key>StandardErrorPath</key><string>{log}</string></dict></plist>\n")
}

pub(super) fn install(url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    install_service(url, listen, cache, limit, home)
}
pub(super) fn install_all(listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    install_service("--all", listen, cache, limit, home)
}
fn install_service(url: &str, listen: &str, cache: &Path, limit: u64, home: &Path) -> Result<()> {
    let label = LABEL;
    if !cfg!(target_os = "macos") {
        bail!("mount --install requires macOS");
    }
    let previous = installed(home)?.first().map(|(url, _)| url.clone());
    let exe = std::env::current_exe()?.canonicalize()?;
    let agent = home.join(format!("Library/LaunchAgents/{label}.plist"));
    std::fs::create_dir_all(agent.parent().unwrap())?;
    std::fs::create_dir_all(home.join("Library/Logs"))?;
    let text = plist(&exe, url, listen, cache, limit, home);
    crate::fsutil::atomic_write(&agent, text.as_bytes(), 0o600)?;
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
    if previous.as_deref().is_some_and(|old| old != url) {
        let _ = Command::new("/sbin/umount").arg(home.join("Layr")).output();
    }
    let output = Command::new("/bin/launchctl").args(["bootstrap", &domain]).arg(&agent).output()?;
    if !output.status.success() {
        bail!("launchctl could not start the disk agent");
    }
    if let Some(message) = replacement(previous.as_deref(), url) {
        println!("{message}");
    }
    println!("The disk starts at login and mounts at {}", home.join("Layr").display());
    Ok(())
}

pub(super) fn remove(home: &Path) -> Result<i32> {
    remove_service(home, false)
}
pub(super) fn remove_all(home: &Path) -> Result<i32> {
    remove_service(home, true)
}
fn remove_service(home: &Path, all: bool) -> Result<i32> {
    let label = LABEL;
    if !cfg!(target_os = "macos") {
        bail!("mount --remove requires macOS");
    }
    let mode = installed(home)?.first().map(|(url, _)| url.clone());
    if !can_remove(mode.as_deref(), all) {
        eprintln!("installed mount mode: {}", mode_label(mode.as_deref()));
        return Ok(2);
    }
    let agent = home.join(format!("Library/LaunchAgents/{label}.plist"));
    if !agent.exists() {
        return Ok(0);
    }
    let domain = format!("gui/{}", rustix::process::getuid().as_raw());
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{label}")]).output();
    let _ = Command::new("/sbin/umount").arg(home.join("Layr")).output();
    std::fs::remove_file(agent)?;
    println!("The disk no longer starts at login");
    Ok(0)
}

pub(super) fn mode_label(mode: Option<&str>) -> String {
    match mode {
        Some("--all") => "all stores".into(),
        Some(url) => format!("single store {url}"),
        None => "none".into(),
    }
}
fn can_remove(mode: Option<&str>, all: bool) -> bool {
    match (mode, all) {
        (Some("--all"), true) => true,
        (Some("--all"), false) => false,
        (None, true) => false,
        (_, false) => true,
        _ => false,
    }
}
fn replacement(previous: Option<&str>, url: &str) -> Option<String> {
    match (previous, url) {
        (Some(old), "--all") if old != "--all" => Some(format!("replaced the single-store mount of {old} with all stores")),
        (Some("--all"), url) if url != "--all" => Some(format!("replaced all stores with the single-store mount of {url}")),
        _ => None,
    }
}

// Read the service arguments that install wrote, as Desk did.
pub(super) fn installed(home: &Path) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for label in [LABEL] {
        let text = match std::fs::read_to_string(home.join(format!("Library/LaunchAgents/{label}.plist"))) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let Some((_, args)) = text.split_once("<key>ProgramArguments</key>") else { continue };
        let args: Vec<_> = args
            .split("</array>")
            .next()
            .unwrap_or("")
            .split("<string>")
            .skip(1)
            .filter_map(|s| {
                s.split_once("</string>").map(|(s, _)| {
                    s.replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&quot;", "\"")
                        .replace("&apos;", "'")
                        .replace("&amp;", "&")
                })
            })
            .collect();
        let url = args.get(2).context("invalid mount agent arguments")?;
        let listen = args.windows(2).find(|pair| pair[0] == "--listen").map(|pair| pair[1].clone()).unwrap_or_default();
        result.push((url.clone(), listen));
    }
    Ok(result)
}

pub(super) fn mount(url: &str, at: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("--at requires the macOS WebDAV client");
    }
    let mounts = Command::new("/sbin/mount").output()?;
    if String::from_utf8_lossy(&mounts.stdout).contains(&format!("{url} on {} (", at.display())) {
        return Ok(());
    }
    std::fs::create_dir_all(at)?;
    let output = Command::new("/sbin/mount_webdav")
        .args(["-S", "-v", "Layr", url])
        .arg(at)
        .output()
        .context("cannot start the macOS WebDAV client")?;
    if !output.status.success() {
        bail!("the macOS WebDAV client could not mount the disk");
    }
    eprintln!("layr mount: mounted at {}", at.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_arguments_are_xml_and_contain_no_paper_key_input() {
        let text =
            plist(Path::new("/A & B/layr"), "file:///a<b", "127.0.0.1:8095", Path::new("/cache"), 17, Path::new("/home/u"));
        assert!(text.contains("/A &amp; B/layr"));
        assert!(text.contains("file:///a&lt;b"));
        assert!(text.contains("/home/u/Layr"));
        assert!(!text.contains("paper-key"));
    }
    #[test]
    fn all_store_arguments_round_trip_without_a_secret() {
        let home = std::env::temp_dir().join(crate::ids::new_id());
        let agent = home.join("Library/LaunchAgents/ws.layr.mount.plist");
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        std::fs::write(&agent, plist(Path::new("/bin/layr"), "--all", "127.0.0.1:8095", Path::new("/cache"), 17, &home)).unwrap();
        assert_eq!(installed(&home).unwrap(), vec![("--all".into(), "127.0.0.1:8095".into())]);
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn one_service_replaces_modes_and_remove_checks_the_mode() {
        assert_eq!(
            replacement(Some("file:///one"), "--all").as_deref(),
            Some("replaced the single-store mount of file:///one with all stores")
        );
        assert_eq!(
            replacement(Some("--all"), "file:///one").as_deref(),
            Some("replaced all stores with the single-store mount of file:///one")
        );
        assert!(can_remove(Some("--all"), true));
        assert!(!can_remove(Some("file:///one"), true));
        assert!(!can_remove(None, true));
        assert!(!can_remove(Some("--all"), false));
        assert!(can_remove(Some("file:///one"), false));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn remove_all_leaves_a_single_store_agent_untouched() {
        let home = std::env::temp_dir().join(crate::ids::new_id());
        let agent = home.join("Library/LaunchAgents/ws.layr.mount.plist");
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        let text = plist(Path::new("/bin/layr"), "file:///one", "127.0.0.1:8095", Path::new("/cache"), 17, &home);
        std::fs::write(&agent, &text).unwrap();
        assert_eq!(remove_all(&home).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&agent).unwrap(), text);
        std::fs::remove_dir_all(home).unwrap();
    }
}
