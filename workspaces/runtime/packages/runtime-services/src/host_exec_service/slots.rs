//! Mac slots: leases, generations, the disk budget and guest recovery.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use super::protocol::{self, Failure, HostExecError, INVALID, Outcome, require};
use super::{FEATURES, GENERATION_COUNTER, HostService, LIST_LIMIT, Op, now, string_list, text};

/// The folder names of `layr-projects/`: a letter or digit, then up to 99
/// letters, digits, `_` or `-`.
pub(crate) fn project_folder_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && value.len() <= 100
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The folders of `path`, as Python `Path.iterdir` with `is_dir()` lists them.
fn folders(path: &Path) -> Outcome<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(path)? {
        let path = entry?.path();
        if path.is_dir() {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn leased(slot: &Path) -> bool {
    slot.join("lease.json").exists()
}

impl HostService {
    pub(crate) fn used_at(slot: &Path) -> Result<f64, HostExecError> {
        let recovery = || {
            HostExecError::new(
                "slot_invalid",
                format!("The host slot metadata needs recovery: {}", slot.display()),
            )
        };
        let text = fs::read_to_string(slot.join("slot.json")).map_err(|_| recovery())?;
        let metadata: Value = serde_json::from_str(&text).map_err(|_| recovery())?;
        require(
            metadata.get("generation").is_some_and(Value::is_string),
            &format!("The host slot metadata is invalid: {}", slot.display()),
            "slot_invalid",
        )?;
        let number = metadata.get("generationNumber");
        require(
            number.is_none_or(Value::is_null)
                || super::integer(number).is_some_and(|number| number >= 1),
            &format!(
                "The host slot generation number is invalid: {}",
                slot.display()
            ),
            "slot_invalid",
        )?;
        match metadata.get("usedAt") {
            None => Ok(0.0),
            Some(Value::Number(used)) => used.as_f64().ok_or_else(recovery),
            Some(Value::Bool(used)) => Ok(f64::from(u8::from(*used))),
            Some(_) => Err(recovery()),
        }
    }

    /// A new slot generation: a random ID and the next number of the project. A
    /// slot made before generation numbers has none until its next new generation.
    pub(crate) fn new_generation(project: &Path) -> Outcome<Map<String, Value>> {
        let counter = project.join(GENERATION_COUNTER);
        let recovery = || {
            HostExecError::new(
                "slot_invalid",
                format!(
                    "The slot generation counter needs recovery: {}",
                    counter.display()
                ),
            )
        };
        let mut last = match fs::read_to_string(&counter) {
            Ok(text) => {
                let value: Value = serde_json::from_str(&text).map_err(|_| recovery())?;
                let last = value.get("last").ok_or_else(recovery)?;
                super::integer(Some(last))
                    .filter(|last| *last >= 0)
                    .ok_or_else(|| {
                        HostExecError::new(
                            "slot_invalid",
                            format!(
                                "The slot generation counter is invalid: {}",
                                counter.display()
                            ),
                        )
                    })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(_) => return Err(recovery().into()),
        };
        for entry in fs::read_dir(project)?.flatten() {
            let number = fs::read_to_string(entry.path().join("slot.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| super::integer(value.get("generationNumber")));
            if let Some(number) = number {
                last = last.max(number);
            }
        }
        protocol::atomic(&counter, &json!({"last": last + 1}))?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| Failure::Unexpected(error.to_string()))?;
        let mut generation = Map::new();
        generation.insert("generation".into(), json!(protocol::hex(&random)));
        generation.insert("generationNumber".into(), json!(last + 1));
        Ok(generation)
    }

    fn require_no_retirement(project: &Path) -> Outcome<()> {
        let interrupted = fs::read_dir(project)?.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".retiring-") && name.ends_with(".json"))
        });
        require(
            !interrupted,
            &format!(
                "An interrupted host slot cleanup needs recovery: {}",
                project.display()
            ),
            "slot_cleanup_failed",
        )?;
        Ok(())
    }

    /// Record the retirement outside the tree before deletion can remove its
    /// metadata. A failed or interrupted deletion must not make the remaining
    /// tree reusable; its marker keeps the old generation for a recovery.
    pub(crate) fn remove_slot(slot: &Path) -> Outcome<()> {
        require(!leased(slot), "The host slot is leased", "busy")?;
        Self::used_at(slot)?;
        let name = slot
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let marker = slot.with_file_name(format!(".retiring-{name}.json"));
        let metadata = protocol::read_json(&slot.join("slot.json"))?;
        protocol::atomic(&marker, &json!({"slotId": name, "metadata": metadata}))?;
        fs::remove_dir_all(slot)
            .and_then(|()| fs::remove_file(&marker))
            .map_err(|error| {
                let text: String = format!(
                    "Host slot cleanup stopped (errno {}); preserve its remaining files and recover it: {}",
                    error.raw_os_error().unwrap_or(0),
                    slot.display()
                )
                .chars()
                .take(1000)
                .collect();
                HostExecError::new("slot_cleanup_failed", text)
            })?;
        Ok(())
    }

    /// The bytes of the files below `path`, as `st_blocks` counts them.
    pub(crate) fn size(path: &Path) -> u64 {
        let mut total = 0_u64;
        let mut stack = vec![path.to_path_buf()];
        while let Some(directory) = stack.pop() {
            let Ok(listing) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in listing.flatten() {
                let path = entry.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                } else if meta.file_type().is_symlink() && path.is_dir() {
                    // os.walk lists a link to a folder with the folders and does not count it.
                } else {
                    total = total.saturating_add(meta.blocks().saturating_mul(512));
                }
            }
        }
        total
    }

    /// Remove idle slots, least recently used first, until `reserve` more bytes
    /// fit in the budget. Returns the bytes in use.
    pub(crate) fn evict(
        &self,
        project: &Path,
        budget: u64,
        reserve: u64,
        exclude: Option<&Path>,
    ) -> Outcome<u64> {
        require(
            reserve <= budget,
            "The sources exceed the project disk budget",
            "disk_budget",
        )?;
        Self::require_no_retirement(project)?;
        let mut candidates = Vec::new();
        for slot in folders(project)? {
            if Some(slot.as_path()) != exclude && !leased(&slot) {
                let used = Self::used_at(&slot)?;
                candidates.push((used, slot));
            }
        }
        candidates.sort_by(|left, right| left.0.total_cmp(&right.0));
        let mut used = Self::size(project);
        let mut candidates = candidates.into_iter();
        while used.saturating_add(reserve) > budget {
            let Some((_, victim)) = candidates.next() else {
                break;
            };
            let size = Self::size(&victim);
            Self::remove_slot(&victim)?;
            used = used.saturating_sub(size);
        }
        self.inner().usage.insert(project.to_path_buf(), used);
        require(
            used.saturating_add(reserve) <= budget,
            "The project disk budget has no free slot capacity",
            "disk_budget",
        )?;
        Ok(used)
    }

    /// Room for a new file. The estimate grows with each file; a full walk of
    /// the project folder happens only when the estimate reaches the budget.
    pub(crate) fn reserve(
        &self,
        project: &Path,
        budget: u64,
        length: u64,
        exclude: &Path,
    ) -> Outcome<()> {
        let known = self.inner().usage.get(project).copied();
        let used = match known {
            Some(used) if used.saturating_add(length) <= budget => used,
            _ => self.evict(project, budget, length, Some(exclude))?,
        };
        self.inner()
            .usage
            .insert(project.to_path_buf(), used.saturating_add(length));
        Ok(())
    }

    pub(crate) fn slot(&self, op: &Op) -> Outcome<PathBuf> {
        let slot_id = match op.get("slotId") {
            Some(Value::String(slot)) => slot.clone(),
            Some(other) => other.to_string(),
            None => return Err(Failure::Unexpected("'slotId'".into())),
        };
        let path = self
            .pool
            .join(protocol::key(text(op, "projectId")))
            .join(slot_id);
        let lease = path.join("lease.json");
        let owned = lease.is_file()
            && protocol::read_json(&lease)?.get("operationId") == op.get("operationId");
        require(owned, "The operation no longer owns its slot", "lease_lost")?;
        Ok(path)
    }

    pub(crate) async fn acquire(self: &Arc<Self>, params: &Map<String, Value>) -> Outcome<Op> {
        let identity = params
            .get("operationId")
            .ok_or_else(|| Failure::Unexpected("'operationId'".into()))?;
        let identity = identity
            .as_str()
            .filter(|identity| !identity.is_empty() && identity.chars().count() <= 128)
            .ok_or_else(|| HostExecError::new(INVALID, "Invalid operation ID"))?
            .to_owned();
        for field in ["projectId", "agentId", "linePath"] {
            require(
                params
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty() && value.chars().count() <= 4096),
                &format!("Missing host command context: {field}"),
                INVALID,
            )?;
        }
        let owner = match params.get("owner") {
            None | Some(Value::Null) => None,
            Some(Value::String(owner)) if !owner.is_empty() && owner.chars().count() <= 128 => {
                Some(owner.clone())
            }
            Some(_) => return Err(HostExecError::new(INVALID, "Invalid guest owner").into()),
        };
        let project_id = text(params, "projectId").to_owned();
        let settings = self.settings(&project_id)?;
        let project = self.pool.join(protocol::key(&project_id));
        let lock = self.lock_for(&project.display().to_string());
        let _held = lock.lock().await;
        if !project.is_dir() {
            super::private_dir(&project)?;
        }
        require(
            !self.opdir(&identity).exists(),
            "The operation already exists",
            "outcome_unknown",
        )?;
        self.evict(&project, settings.disk_budget, 0, None)?;
        let mut existing = folders(&project)?;
        let mut free = Vec::new();
        for slot in &existing {
            if !leased(slot) {
                free.push((Self::used_at(slot)?, slot.clone()));
            }
        }
        free.sort_by(|left, right| left.0.total_cmp(&right.0));
        let mut free: Vec<PathBuf> = free.into_iter().map(|(_, slot)| slot).collect();
        // A smaller slot count removes idle slots, the least recently used first.
        while i64::try_from(existing.len()).unwrap_or(i64::MAX) > settings.slots && !free.is_empty()
        {
            let victim = free.remove(0);
            Self::remove_slot(&victim)?;
            existing.retain(|slot| slot != &victim);
        }
        let slot = if let Some(slot) = free.pop() {
            slot
        } else {
            require(
                i64::try_from(existing.len()).unwrap_or(i64::MAX) < settings.slots,
                "All project slots are leased; host_exec action list shows your operations",
                "busy",
            )?;
            let slot = (0..settings.slots)
                .map(|index| project.join(index.to_string()))
                .find(|slot| !slot.exists())
                .ok_or_else(|| {
                    HostExecError::new(
                        "busy",
                        "All project slots are leased; host_exec action list shows your operations",
                    )
                })?;
            super::private_dir(&slot)?;
            let mut metadata = Self::new_generation(&project)?;
            metadata.insert("usedAt".into(), json!(0));
            protocol::atomic(&slot.join("slot.json"), &Value::Object(metadata))?;
            slot
        };
        for name in ["source", "DerivedData", "packages"] {
            let folder = slot.join(name);
            if !folder.is_dir() {
                super::private_dir(&folder)?;
            }
        }
        let directory = self.opdir(&identity);
        super::private_dirs(directory.parent().unwrap_or(&self.state))?;
        super::private_dir(&directory)?;
        let artifact_dir = slot.join("artifacts").join(protocol::key(&identity));
        super::private_dirs(&artifact_dir)?;
        std::os::unix::fs::symlink(&artifact_dir, directory.join("artifacts"))?;
        let meta = protocol::read_json(&slot.join("slot.json"))?;
        let slot_name = slot
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let mut op = Op::new();
        op.insert("operationId".into(), json!(identity));
        for field in ["projectId", "agentId", "linePath"] {
            op.insert(
                field.into(),
                params.get(field).cloned().unwrap_or(Value::Null),
            );
        }
        op.insert("slotId".into(), json!(slot_name));
        op.insert(
            "generation".into(),
            meta.get("generation").cloned().unwrap_or(Value::Null),
        );
        if let Some(number) = meta.get("generationNumber") {
            op.insert("generationNumber".into(), number.clone());
        }
        op.insert("state".into(), json!("leased"));
        op.insert("createdAt".into(), json!(now()));
        op.insert("slotPath".into(), json!(slot.join("source")));
        op.insert("derivedData".into(), json!(slot.join("DerivedData")));
        op.insert("packageCache".into(), json!(slot.join("packages")));
        op.insert("artifactsPath".into(), json!(directory.join("artifacts")));
        op.insert("diskBudget".into(), json!(settings.disk_budget));
        op.insert("macOnly".into(), json!(settings.mac_only));
        op.insert("features".into(), json!(FEATURES));
        if let Some(owner) = owner {
            op.insert("owner".into(), json!(owner));
        }
        self.save(&identity, &op)?;
        protocol::atomic(&slot.join("lease.json"), &json!({"operationId": identity}))?;
        Ok(op)
    }

    pub(crate) fn release(
        &self,
        identity: &str,
        mut op: Op,
        slot: &Path,
        params: &Map<String, Value>,
    ) -> Outcome<Op> {
        let state = text(&op, "state");
        require(
            state == "leased" || state == "exited",
            "A running or unknown command retains its lease",
            INVALID,
        )?;
        let failure = params
            .get("preRunError")
            .filter(|failure| !failure.is_null());
        if let Some(failure) = failure {
            require(
                text(&op, "state") == "leased" && failure.is_object(),
                "Only a failed transfer can release an unstarted command",
                INVALID,
            )?;
            op.insert("reason".into(), json!("sync_failed"));
            op.insert("preRunError".into(), failure.clone());
        }
        let mut abandoned = None;
        if studio_runtime_store::truthy(params.get("abandon")) {
            // The agent gives up an operation that no guest request owns any more.
            require(
                !(text(&op, "state") == "leased" && self.is_locked(&format!("run\0{identity}"))),
                "The command is starting; release it after it exits",
                "busy",
            )?;
            abandoned = Some("agent_release");
        }
        self.release_operation(
            identity,
            &mut op,
            slot,
            studio_runtime_store::truthy(params.get("collected")),
            failure.is_some(),
            abandoned,
        )?;
        Ok(op)
    }

    /// Free the Mac slot of a leased or exited operation. Its saved results stay
    /// readable unless the guest collected them. An abandoned operation gets a new
    /// slot generation: no guest request owns its guest staging any more.
    pub(crate) fn release_operation(
        &self,
        identity: &str,
        op: &mut Op,
        slot: &Path,
        collected: bool,
        mut new_generation: bool,
        abandoned: Option<&str>,
    ) -> Outcome<()> {
        if let Some(abandoned) = abandoned {
            op.insert("abandoned".into(), json!(abandoned));
            new_generation = true;
        }
        op.insert("state".into(), json!("released"));
        op.insert("collected".into(), json!(collected));
        op.insert("finishedAt".into(), json!(now()));
        self.save(identity, op)?;
        let project = slot.parent().unwrap_or(slot);
        // Failed guest staging can stay active after a lost broker response. Never reuse it.
        let mut generation = if new_generation {
            Self::new_generation(project)?
        } else {
            ["generation", "generationNumber"]
                .into_iter()
                .filter_map(|field| op.get(field).map(|value| (field.to_owned(), value.clone())))
                .collect()
        };
        generation.insert("usedAt".into(), json!(now()));
        protocol::atomic(&slot.join("slot.json"), &Value::Object(generation))?;
        fs::remove_file(slot.join("lease.json"))?;
        let directory = self.opdir(identity);
        if collected {
            super::remove_tree_quietly(&directory.join("changes"));
        }
        // A large slot has a large manifest and one entry file per transferred file.
        match fs::remove_file(directory.join("held-manifest.json")) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        super::remove_tree_quietly(&directory.join("entries"));
        let pending = {
            let mut inner = self.inner();
            for name in ["held", "changes", "artifacts"] {
                inner.pages.remove(&format!("{identity}\0{name}"));
            }
            inner.cloned.remove(slot)
        };
        self.forget_lock(&format!("run\0{identity}"));
        if let Some(pending) = pending.filter(|pending| !pending.is_empty()) {
            // Clones of a command that did not start keep their hashes for the next scan.
            let cache = slot.join("hash-cache.json");
            let mut hashes = protocol::load_hashes(&cache);
            hashes.extend(pending);
            protocol::atomic(&cache, &Value::Object(hashes))?;
        }
        self.evict(project, self.budget_of(op), 0, None)?;
        Ok(())
    }

    /// A command whose guest owner is gone never starts: nobody would collect its result.
    pub(crate) fn refuse_orphan(&self, identity: &str) -> Outcome<()> {
        let mut op = self.load(identity)?;
        if studio_runtime_store::truthy(op.get("orphaned")) && text(&op, "state") == "leased" {
            let slot = self.slot(&op)?;
            self.release_operation(
                identity,
                &mut op,
                &slot,
                false,
                false,
                Some("guest_owner_lost"),
            )?;
            return Err(HostExecError::new(
                "guest_owner_lost",
                "The guest that sent this command is gone; the command did not start",
            )
            .into());
        }
        Ok(())
    }

    /// The operation and the slot of each Mac slot lease.
    fn leases(&self) -> Vec<(String, PathBuf)> {
        let mut found = Vec::new();
        let Ok(projects) = folders(&self.pool) else {
            return found;
        };
        for project in projects {
            let Ok(mut slots) = fs::read_dir(&project).map(|listing| {
                listing
                    .flatten()
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            }) else {
                continue;
            };
            slots.sort();
            for slot in slots {
                let identity = fs::read_to_string(slot.join("lease.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| {
                        value
                            .get("operationId")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                if let Some(identity) = identity {
                    found.push((identity, slot));
                }
            }
        }
        found
    }

    /// The newest operations of one agent, the ones that hold a Mac slot first.
    /// The agent finds an operation here when it lost the result that named it.
    pub(crate) fn operations(&self, agent: Option<&Value>) -> Outcome<Value> {
        let agent = agent
            .and_then(Value::as_str)
            .filter(|agent| !agent.is_empty())
            .ok_or_else(|| HostExecError::new(INVALID, "Missing host command context: agentId"))?;
        let leased: std::collections::HashSet<String> = self
            .leases()
            .into_iter()
            .map(|(identity, _)| identity)
            .collect();
        let mut rows = Vec::new();
        let listing = match fs::read_dir(self.state.join("host-operations")) {
            Ok(listing) => listing.flatten().collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        for entry in listing {
            let Some(value) = fs::read_to_string(entry.path().join("status.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            else {
                continue;
            };
            let Some(identity) = value.get("operationId").and_then(Value::as_str) else {
                continue;
            };
            if value.get("agentId").and_then(Value::as_str) != Some(agent) {
                continue;
            }
            let op = self.load(identity)?;
            let mut row = Map::new();
            row.insert("operationId".into(), json!(identity));
            row.insert("leased".into(), json!(leased.contains(identity)));
            for field in [
                "state",
                "reason",
                "exitCode",
                "createdAt",
                "startedAt",
                "exitedAt",
                "finishedAt",
                "slotId",
                "collected",
                "abandoned",
                "orphaned",
            ] {
                if let Some(value) = op.get(field) {
                    row.insert(field.into(), value.clone());
                }
            }
            rows.push(row);
        }
        rows.sort_by(|left, right| {
            let leased = |row: &Map<String, Value>| !row["leased"].as_bool().unwrap_or(false);
            let created = |row: &Map<String, Value>| {
                row.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0)
            };
            leased(left)
                .cmp(&leased(right))
                .then_with(|| created(right).total_cmp(&created(left)))
        });
        let total = rows.len();
        rows.truncate(LIST_LIMIT);
        Ok(json!({"operations": rows, "total": total}))
    }

    /// A new guest service owns no operation of an earlier one: the guest has one
    /// service at a time, and its requests die with its process. Exited operations
    /// of an earlier owner free their slots now. A running command keeps its slot
    /// until it exits. An unknown outcome of a lost host owner keeps its slot.
    pub(crate) fn recover(&self, owner: Option<&Value>) -> Outcome<Value> {
        let owner = owner
            .and_then(Value::as_str)
            .filter(|owner| !owner.is_empty() && owner.chars().count() <= 128)
            .ok_or_else(|| HostExecError::new(INVALID, "Invalid guest owner"))?;
        let (mut released, mut orphaned, mut kept, mut released_slots) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (identity, slot) in self.leases() {
            let mut op = match self.load(&identity) {
                Ok(op) => op,
                Err(Failure::Refused(_)) => {
                    kept.push(identity);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if op.get("owner").and_then(Value::as_str) == Some(owner) {
                continue;
            }
            let state = text(&op, "state").to_owned();
            let starting = self.is_locked(&format!("run\0{identity}"));
            if state == "exited" || (state == "leased" && !starting) {
                self.release_operation(
                    &identity,
                    &mut op,
                    &slot,
                    false,
                    false,
                    Some("guest_owner_lost"),
                )?;
                released.push(identity);
                // The guest forgets the guest slot lease of the old staging (host_exec.py, recover).
                let fields: Map<String, Value> = [
                    "operationId",
                    "agentId",
                    "slotId",
                    "generation",
                    "generationNumber",
                ]
                .into_iter()
                .filter_map(|field| op.get(field).map(|value| (field.to_owned(), value.clone())))
                .collect();
                released_slots.push(Value::Object(fields));
            } else if state == "running" || state == "leased" {
                // The command or its start finishes first; then the slot is free.
                op.insert("orphaned".into(), json!(true));
                self.save(&identity, &op)?;
                orphaned.push(identity);
            } else {
                kept.push(identity);
            }
        }
        Ok(
            json!({"released": released, "orphaned": orphaned, "kept": kept, "releasedSlots": released_slots}),
        )
    }

    pub(crate) fn mac_only(op: &Op) -> Vec<String> {
        string_list(op, "macOnly")
    }
}
