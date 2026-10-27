//! Discover stores from caller-readable project metadata and local pins.
use super::*;

#[derive(Clone)]
pub(super) struct Known {
    pub name: String,
    pub url: String,
    pub pin: Option<StorePin>,
    pub profile: Option<String>,
}

fn value(db: &rusqlite::Connection, key: &str) -> Result<Option<serde_json::Value>> {
    use rusqlite::OptionalExtension;
    let text: Option<String> = db.query_row("SELECT value FROM config WHERE key=?1", [key], |r| r.get(0)).optional()?;
    text.map(|s| serde_json::from_str(&s).context("invalid remote config")).transpose()
}

pub(super) fn known(home: &Path) -> Result<Vec<Known>> {
    let mut stores = BTreeMap::<String, Known>::new();
    let projects = crate::store::default_root().join("projects");
    if let Ok(dirs) = std::fs::read_dir(projects) {
        let mut dirs: Vec<_> = dirs.collect::<std::io::Result<_>>()?;
        dirs.sort_by_key(|e| e.file_name());
        for entry in dirs {
            // The caller can only discover metadata that its filesystem access permits.
            let db = match rusqlite::Connection::open_with_flags(
                crate::store::db_file(&entry.path()),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                Ok(db) => db,
                Err(_) => continue,
            };
            let mut statement = db.prepare("SELECT key FROM config WHERE key LIKE 'cloud.remote.%.url' ORDER BY key")?;
            let keys = statement.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for key in keys {
                let name = key.strip_prefix("cloud.remote.").unwrap().strip_suffix(".url").unwrap();
                let Some(url) = value(&db, &key)?.and_then(|v| v.as_str().map(str::to_owned)) else { continue };
                if !["file://", "s3://", "layr+https://"].iter().any(|p| url.starts_with(p)) {
                    continue;
                }
                let pin = value(&db, &format!("cloud.remote.{name}.store"))?.map(serde_json::from_value).transpose()?;
                let profile = value(&db, &format!("remote.{name}.aws-profile"))?.and_then(|v| v.as_str().map(str::to_owned));
                if let Some(old) = stores.get_mut(&url) {
                    if old.pin.is_some() && pin.is_some() && old.pin != pin {
                        bail!("conflicting local pins for store {url}");
                    }
                    if old.pin.is_none() {
                        old.pin = pin;
                    }
                } else {
                    stores.insert(url.clone(), Known { name: line_folder(name), url, pin, profile });
                }
            }
        }
    }
    for (url, pin) in load_pins(&pins_file(home))? {
        if let Some(old) = stores.get_mut(&url) {
            if old.pin.as_ref().is_some_and(|p| p != &pin) {
                bail!("conflicting local pins for store {url}");
            }
            old.pin = Some(pin);
        } else {
            let name = format!("project-{}", crate::ids::short(&pin.project));
            stores.insert(url.clone(), Known { name, url, pin: Some(pin), profile: None });
        }
    }
    let (person, addresses) = person_stores(home)?;
    if let Some(person) = person {
        for pin in person.index.vaults {
            let Some(address) = addresses.get(&pin.project).filter(|a| a.pin == pin) else { continue };
            let pin = StorePin { project: pin.project, recipient: pin.recipient, paper_recipient: None };
            if let Some(old) = stores.get_mut(&address.url) {
                if old.pin.as_ref().is_some_and(|old| old != &pin) {
                    bail!("conflicting local pins for store {}", address.url);
                }
                old.pin = Some(pin);
            } else {
                let name = format!("{}-{}", line_folder(&address.name), crate::ids::short(&pin.project));
                stores.insert(address.url.clone(), Known { name, url: address.url.clone(), pin: Some(pin), profile: None });
            }
        }
    }
    let mut names = BTreeSet::new();
    let mut result: Vec<_> = stores.into_values().collect();
    for store in &mut result {
        let base = store.name.clone();
        let mut n = 2;
        while !names.insert(store.name.clone()) {
            store.name = format!("{base}-{n}");
            n += 1;
        }
    }
    Ok(result)
}

fn person_stores(home: &Path) -> Result<(Option<crate::person::Access>, BTreeMap<String, crate::person::StoreAddress>)> {
    let env: HashMap<String, String> = std::env::vars().collect();
    if !env.contains_key("LAYR_PERSON_KEY_FILE") {
        return Ok((None, BTreeMap::new()));
    }
    let dir = crate::person::folder(&env, home);
    let login = env.get("LAYR_PERSON_LOGIN").cloned().or_else(|| std::fs::read_to_string(dir.join("login.txt")).ok());
    let uid = rustix::process::getuid().as_raw();
    let person = crate::person::load(&env, home, &std::env::current_dir()?, uid, login.as_deref())?;
    Ok((person, crate::person::addresses(&dir, uid)?))
}

pub(super) fn profile_env(remote: &str, url: &str, home: &Path, env: &mut HashMap<String, String>) -> Result<()> {
    if !url.starts_with("s3://") {
        return Ok(());
    }
    if !env.get("AWS_PROFILE").is_some_and(|v| !v.is_empty()) {
        let project = std::env::var("LAYR_PROJECT").ok().or_else(|| {
            std::env::current_dir()
                .ok()?
                .strip_prefix(crate::store::default_root().join("projects"))
                .ok()?
                .components()
                .next()
                .map(|p| p.as_os_str().to_string_lossy().into_owned())
        });
        if let Some(project) = project.filter(|_| !remote.contains("://")) {
            let db = rusqlite::Connection::open_with_flags(
                crate::store::db_file(&crate::store::default_root().join("projects").join(project)),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            if let Some(profile) =
                value(&db, &format!("remote.{remote}.aws-profile"))?.and_then(|v| v.as_str().map(str::to_owned))
            {
                env.insert("AWS_PROFILE".into(), profile);
            }
        } else if let Some(profile) = known(home)?.into_iter().find(|s| s.url == url).and_then(|s| s.profile) {
            env.insert("AWS_PROFILE".into(), profile);
        }
    }
    Ok(())
}

pub(super) fn status(home: &Path, json: bool) -> Result<()> {
    let mut stores = known(home)?;
    let installed = launch::installed(home)?;
    for (url, _) in &installed {
        if url != "--all" && !stores.iter().any(|s| &s.url == url) {
            stores.push(Known { name: url.clone(), url: url.clone(), pin: None, profile: None });
        }
    }
    let active = active(home)?;
    let mut rows = status_rows(stores, &installed, &active)?;
    let (person, addresses) = person_stores(home)?;
    if let Some(person) = person {
        for pin in person.index.vaults {
            if addresses.get(&pin.project).is_some_and(|a| a.pin == pin) {
                continue;
            }
            let message = "no address: run layr person accept again with --remote <url>";
            rows.push(serde_json::json!({"name":format!("project-{}",crate::ids::short(&pin.project)),"project":pin.project,"url":null,"status":message,"installed":false,"installed_mode":launch::mode_label(installed.first().map(|(url,_)|url.as_str())),"address":null}));
        }
    }
    if json {
        println!(
            "{}",
            serde_json::json!({"installed_mode":launch::mode_label(installed.first().map(|(url,_)|url.as_str())),"stores":rows})
        );
    } else {
        if rows.is_empty() {
            println!("installed mount mode: {}", launch::mode_label(installed.first().map(|(url, _)| url.as_str())));
        }
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t{}",
                row["url"].as_str().or_else(|| row["project"].as_str()).unwrap_or(""),
                row["status"].as_str().unwrap(),
                if row["installed"] == true { "installed" } else { "not installed" },
                row["address"].as_str().unwrap_or(""),
                row["installed_mode"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

fn status_rows(
    stores: Vec<Known>,
    installed: &[(String, String)],
    active: &[(Vec<String>, String)],
) -> Result<Vec<serde_json::Value>> {
    let mut rows = Vec::new();
    let installed_mode = installed.first().map(|(url, _)| url.as_str());
    for store in stores {
        let unlocked = keystore::get(&keystore::account(&store.url))?.is_some();
        let installed = installed.iter().any(|(url, _)| url == "--all" || url == &store.url);
        let address =
            active.iter().find(|(urls, _)| urls.iter().any(|url| url == "--all" || url == &store.url)).map(|(_, addr)| addr);
        let mode = launch::mode_label(installed_mode);
        rows.push(serde_json::json!({"name":store.name,"url":store.url,"status":if unlocked {"unlocked"} else {"locked"},"installed":installed,"installed_mode":mode,"address":address}));
    }
    Ok(rows)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Active {
    pid: u32,
    urls: Vec<String>,
    address: String,
}
pub(super) struct Serving(PathBuf);
impl Drop for Serving {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub(super) fn serving(home: &Path, urls: Vec<String>, address: String) -> Result<Serving> {
    let path = preferences_file(home).with_file_name(format!("mount-active-{}.json", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap())?;
    crate::fsutil::atomic_write(&path, &serde_json::to_vec(&Active { pid: std::process::id(), urls, address })?, 0o600)?;
    Ok(Serving(path))
}
fn active(home: &Path) -> Result<Vec<(Vec<String>, String)>> {
    let mut result = Vec::new();
    if let Ok(entries) = std::fs::read_dir(preferences_file(home).parent().unwrap()) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with("mount-active-") {
                continue;
            }
            let Some(record) = std::fs::read(entry.path()).ok().and_then(|b| serde_json::from_slice::<Active>(&b).ok()) else {
                continue;
            };
            if rustix::process::Pid::from_raw(record.pid as i32)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
            {
                result.push((record.urls, record.address));
            }
        }
    }
    Ok(result)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn status_reports_the_keychain_and_service_without_the_identity() {
        keystore::test_service();
        let url = format!("file:///layr-status-{}", crate::ids::new_id());
        let store = Known { name: "origin".into(), url: url.clone(), pin: None, profile: None };
        let account = keystore::account(&url);
        let rows = status_rows(vec![store.clone()], &[], &[]).unwrap();
        assert_eq!(rows[0]["status"], "locked");
        assert_eq!(rows[0]["installed"], false);
        assert!(rows[0]["address"].is_null());
        let identity = format!("private-test-{}", crate::ids::new_id());
        keystore::set(&account, &identity).unwrap();
        let rows = status_rows(
            vec![store],
            &[("--all".into(), "127.0.0.1:8095".into())],
            &[(vec!["--all".into()], "http://127.0.0.1:8095/".into())],
        )
        .unwrap();
        assert_eq!(rows[0]["status"], "unlocked");
        assert_eq!(rows[0]["installed"], true);
        assert_eq!(rows[0]["installed_mode"], "all stores");
        assert_eq!(rows[0]["address"], "http://127.0.0.1:8095/");
        assert!(!serde_json::to_string(&rows).unwrap().contains(&identity));
        keystore::remove(&account).unwrap();
    }
}
