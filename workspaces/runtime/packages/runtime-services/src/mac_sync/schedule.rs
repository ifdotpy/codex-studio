//! When Mac folder sync rounds run (docs/vm-mac-sync.md, "Watcher"): a round after the
//! file events of a folder go quiet, a check of main every [`HEAD_INTERVAL`] seconds,
//! and a full scan every [`FULL_INTERVAL`] seconds (every [`HEAD_INTERVAL`] seconds
//! without the file event helper). A stopped VM makes every round wait.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_store::path_identity::CanonicalPath;

use super::{MacSyncError, error_reason, read_json, round::ProjectSync, sync_status};
use crate::linux_vm_guest::GuestRpc;

/// Seconds between checks of the main state of a synced project.
pub const HEAD_INTERVAL: f64 = 10.0;
/// Seconds between full scans of a Mac folder while the file event helper runs.
pub const FULL_INTERVAL: f64 = 600.0;
/// A folder's file events start a round after this much quiet.
pub const QUIET: Duration = Duration::from_secs(2);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_STOP: Duration = Duration::from_secs(5);

/// A VM project with a Mac folder whose sync is not paused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedProject {
    pub project_id: String,
    pub source: PathBuf,
    pub import_state: Option<String>,
}

/// VM projects with a Mac folder whose sync is not paused, in project ID order.
pub fn synced_projects(state_dir: &Path) -> Vec<SyncedProject> {
    let Ok(listing) = std::fs::read_dir(state_dir.join("layr-projects")) else {
        return Vec::new();
    };
    let mut folders = listing
        .filter_map(Result::ok)
        .map(|item| item.path())
        .collect::<Vec<_>>();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|folder| {
            let record = read_json(&folder.join("project.json"), Value::Null).ok()?;
            let enabled = read_json(&folder.join("sync/config.json"), json!({}))
                .map(|config| config["enabled"].as_bool().unwrap_or(true))
                .unwrap_or(true);
            let source = record["source"]
                .as_str()
                .filter(|source| !source.is_empty())?;
            if !enabled || !Path::new(source).is_dir() {
                return None;
            }
            let project_id = match record["projectId"].as_str() {
                Some(id) => id.to_owned(),
                None => folder.file_name()?.to_string_lossy().into_owned(),
            };
            Some(SyncedProject {
                project_id,
                source: PathBuf::from(source),
                import_state: record["importStateId"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// File events of the synced Mac folders.
pub trait FileEvents: Send {
    /// Watch exactly these folders.
    fn watch(&mut self, roots: &[PathBuf]);
    /// Whether events arrive; without them the scheduler scans more often.
    fn available(&mut self) -> bool;
    /// (something changed below `root` and is quiet, its paths or `None` for all).
    fn take(&mut self, root: &Path) -> (bool, Option<BTreeSet<String>>);
}

/// Runs one round of a project; the production runner is [`ProjectRounds`].
pub trait RoundRunner {
    fn round(
        &self,
        project: &ProjectSync<'_>,
        paths: Option<&BTreeSet<String>>,
    ) -> Result<Value, MacSyncError>;
}

/// [`ProjectSync::round`].
pub struct ProjectRounds;

impl RoundRunner for ProjectRounds {
    fn round(
        &self,
        project: &ProjectSync<'_>,
        paths: Option<&BTreeSet<String>>,
    ) -> Result<Value, MacSyncError> {
        project.round(paths)
    }
}

/// When a project's last full scan and main check ran; `-inf` is never.
#[derive(Clone, Copy)]
struct Times {
    full: f64,
    head: f64,
}

impl Default for Times {
    fn default() -> Self {
        Self {
            full: f64::NEG_INFINITY,
            head: f64::NEG_INFINITY,
        }
    }
}

/// The sync work of every synced project, one tick at a time and never twice at once.
pub struct Scheduler<E: FileEvents> {
    events: E,
    timing: BTreeMap<String, Times>,
}

/// The sidebar fields of a project's report; the time of the last round stays out.
fn visible(state_dir: &Path, project_id: &str) -> Option<Value> {
    let mut status = sync_status(state_dir, project_id).ok().flatten()?;
    if let Some(object) = status.as_object_mut() {
        object.remove("at");
    }
    Some(status)
}

impl<E: FileEvents> Scheduler<E> {
    pub fn new(events: E) -> Self {
        Self {
            events,
            timing: BTreeMap::new(),
        }
    }

    pub fn events_mut(&mut self) -> &mut E {
        &mut self.events
    }

    /// Start the due rounds; `now` is a monotonic time in seconds. Returns whether a
    /// report that the sidebar shows changed. A failure stays visible in `status.json`.
    pub fn tick(
        &mut self,
        state_dir: &Path,
        guest: &dyn GuestRpc,
        vm_running: &dyn Fn() -> bool,
        now: f64,
        rounds: &dyn RoundRunner,
    ) -> bool {
        let projects = synced_projects(state_dir);
        let roots = projects
            .iter()
            .map(|project| project.source.clone())
            .collect::<Vec<_>>();
        self.events.watch(&roots);
        if projects.is_empty() || !vm_running() {
            return false;
        }
        let before = projects
            .iter()
            .map(|project| visible(state_dir, &project.project_id))
            .collect::<Vec<_>>();
        for project in &projects {
            let Ok(sync) = ProjectSync::open(guest, state_dir, &project.project_id) else {
                continue;
            };
            let mut times = self
                .timing
                .get(&project.project_id)
                .copied()
                .unwrap_or_default();
            let outcome = self
                .scope(guest, &sync, &mut times, now)
                .and_then(|(due, scope)| {
                    if !due {
                        return Ok(());
                    }
                    rounds.round(&sync, scope.as_ref())?;
                    if scope.is_none() {
                        times.full = now;
                    }
                    Ok(())
                });
            match outcome {
                Ok(()) => {}
                Err(MacSyncError::Busy) => {
                    // A choice holds the project: the next tick scans in full, so the
                    // file event paths are not lost.
                    times.full = f64::NEG_INFINITY;
                }
                Err(error) => {
                    if !matches!(error, MacSyncError::Unproven { .. })
                        && let Err(write) = sync.record_status(
                            json!({"state": "error", "reason": error_reason(&error)}),
                        )
                    {
                        eprintln!(
                            "mac sync {}: the status could not be saved: {}",
                            project.project_id,
                            error_reason(&write)
                        );
                    }
                    times.full = f64::NEG_INFINITY;
                }
            }
            self.timing.insert(project.project_id.clone(), times);
        }
        projects
            .iter()
            .zip(before)
            .any(|(project, before)| visible(state_dir, &project.project_id) != before)
    }

    /// (a round is due, what it reads on the Mac): `None` for all, or a set of paths that
    /// is empty when only main changed.
    fn scope(
        &mut self,
        guest: &dyn GuestRpc,
        project: &ProjectSync<'_>,
        times: &mut Times,
        now: f64,
    ) -> Result<(bool, Option<BTreeSet<String>>), MacSyncError> {
        let full_every = if self.events.available() {
            FULL_INTERVAL
        } else {
            HEAD_INTERVAL
        };
        let (changed, paths) = self.events.take(project.root());
        let status = read_json(&project.directory().join("status.json"), json!({}))?;
        // Mac deletions that wait for the user: only a full scan looks again, and a
        // confirmation starts one at once.
        let guarded = status["state"] == "paused"
            && status["deletes"].as_u64().is_some_and(|count| count > 0);
        if now - times.full >= full_every
            || (changed && guarded)
            || project.directory().join("allow-deletes.json").exists()
        {
            return Ok((true, None));
        }
        if guarded {
            return Ok((false, None));
        }
        if changed {
            return Ok((true, paths));
        }
        if now - times.head < HEAD_INTERVAL {
            return Ok((false, None));
        }
        times.head = now;
        let head = guest
            .call(
                "sync.mac.head",
                &json!({"projectId": project.project_id()}),
                None,
                HEAD_TIMEOUT,
            )
            .map_err(MacSyncError::Guest)?;
        // A conflict or a handoff waits for main or the Mac to change.
        let rescan = status["rescan"]
            .as_array()
            .is_some_and(|rescan| !rescan.is_empty());
        if head["mainStateId"] == status["mainStateId"]
            && !head["mainStateId"].is_null()
            && !rescan
            && matches!(
                status["state"].as_str(),
                Some("synced" | "conflict" | "waiting")
            )
        {
            return Ok((false, None));
        }
        Ok((true, Some(BTreeSet::new())))
    }
}

#[derive(Default)]
struct Shared {
    /// Changed paths per folder; `None` asks for a full scan.
    pending: BTreeMap<PathBuf, Option<BTreeSet<String>>>,
    /// When the last event of a folder arrived; absent is quiet.
    last: BTreeMap<PathBuf, Instant>,
    ready: bool,
    /// The helper process that the reader serves; a newer one ends older readers.
    generation: u64,
}

/// One `studio-fs-events` process for every synced Mac folder (docs/vm-mac-sync.md,
/// "Watcher"). It collects the changed paths per folder; a dropped-events report asks
/// for a full scan of that folder. The helper exits when its input closes.
pub struct Watcher {
    helper: PathBuf,
    quiet: Duration,
    child: Option<Child>,
    input: Option<ChildStdin>,
    roots: Vec<PathBuf>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

impl Watcher {
    pub fn new(helper: PathBuf, quiet: Duration) -> Self {
        Self {
            helper,
            quiet,
            child: None,
            input: None,
            roots: Vec::new(),
            shared: Arc::default(),
        }
    }

    /// Wait until the helper reports that it watches the folders.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let (lock, ready) = &*self.shared;
        let shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ready
            .wait_timeout_while(shared, timeout, |shared| !shared.ready)
            .map(|(shared, _)| shared.ready)
            .unwrap_or(false)
    }

    /// The paths that wait for a folder's next round, for diagnostics; `Some(None)`
    /// asks for a full scan.
    pub fn pending(&self, root: &Path) -> Option<Option<BTreeSet<String>>> {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .get(root)
            .cloned()
    }

    /// Stop the helper: close its input, then kill it after a grace period.
    #[allow(
        clippy::disallowed_methods,
        reason = "the watcher stops its helper on the scheduler's dedicated blocking thread"
    )]
    pub fn stop(&mut self) {
        self.input = None;
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + HELPER_STOP;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        drop(child.kill());
                        drop(child.wait());
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
        let (lock, _) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.ready = false;
        shared.generation += 1;
    }

    fn start(&mut self) {
        let mut command = Command::new(&self.helper);
        command
            .args(&self.roots)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("mac sync: the file event helper did not start: {error}");
                return;
            }
        };
        let Some(output) = child.stdout.take() else {
            drop(child.kill());
            drop(child.wait());
            return;
        };
        self.input = child.stdin.take();
        self.child = Some(child);
        let generation = {
            let (lock, _) = &*self.shared;
            let mut shared = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Whatever happened while no helper watched is unknown: scan in full.
            for root in &self.roots {
                shared.pending.insert(root.clone(), None);
                shared.last.remove(root);
            }
            shared.generation
        };
        let shared = Arc::clone(&self.shared);
        let roots = self.roots.clone();
        let spawned = std::thread::Builder::new()
            .name("studio-fs-events".into())
            .spawn(move || read_events(output, roots, shared, generation));
        if let Err(error) = spawned {
            eprintln!("mac sync: the file event reader did not start: {error}");
            self.stop();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_events(
    output: std::process::ChildStdout,
    roots: Vec<PathBuf>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    generation: u64,
) {
    let names = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<BTreeSet<_>>();
    for line in BufReader::new(output).lines() {
        let Ok(line) = line else {
            return;
        };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (lock, ready) = &*shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation != generation {
            return;
        }
        if let Some(reported) = event["ready"].as_array() {
            let reported = reported
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            if reported == names {
                state.ready = true;
                ready.notify_all();
            }
            continue;
        }
        let rescan = event.get("rescan").is_some();
        let Some(value) = event["path"].as_str().or_else(|| event["rescan"].as_str()) else {
            continue;
        };
        let changed = Path::new(value);
        let Some(root) = roots.iter().find(|root| changed.starts_with(root)) else {
            continue;
        };
        let known = state.pending.get(root).cloned();
        let next = match (known, changed.strip_prefix(root)) {
            (Some(None), _) => None,
            _ if rescan || changed == root.as_path() => None,
            (known, Ok(relative)) => {
                let mut paths = known.flatten().unwrap_or_default();
                paths.insert(relative.to_string_lossy().into_owned());
                Some(paths)
            }
            (_, Err(_)) => None,
        };
        state.pending.insert(root.clone(), next);
        state.last.insert(root.clone(), Instant::now());
    }
}

impl FileEvents for Watcher {
    fn watch(&mut self, roots: &[PathBuf]) {
        let mut wanted = roots
            .iter()
            .map(|root| {
                CanonicalPath::existing(root)
                    .map(|path| path.to_path_buf())
                    .unwrap_or_else(|_| root.clone())
            })
            .collect::<Vec<_>>();
        wanted.sort();
        wanted.dedup();
        if wanted == self.roots && self.available() {
            return;
        }
        self.stop();
        self.roots = wanted;
        if self.roots.is_empty() || !self.helper.is_file() {
            return;
        }
        self.start();
    }

    fn available(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    fn take(&mut self, root: &Path) -> (bool, Option<BTreeSet<String>>) {
        let root = CanonicalPath::existing(root)
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|_| root.to_path_buf());
        let (lock, _) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let quiet = shared
            .last
            .get(&root)
            .is_none_or(|last| last.elapsed() >= self.quiet);
        if !quiet || !shared.pending.contains_key(&root) {
            return (false, Some(BTreeSet::new()));
        }
        (true, shared.pending.remove(&root).flatten())
    }
}

/// The server task: one tick per second on the blocking pool, never two at once. A
/// changed sidebar report wakes the sync clients.
pub async fn run(
    state_dir: PathBuf,
    guest: Arc<dyn GuestRpc>,
    vm_running: Arc<dyn Fn() -> bool + Send + Sync>,
    helper: PathBuf,
    changes: Arc<crate::change_notifications::ChangeNotificationHub>,
) {
    let scheduler = Arc::new(Mutex::new(Scheduler::new(Watcher::new(helper, QUIET))));
    let started = Instant::now();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let scheduler = Arc::clone(&scheduler);
        let guest = Arc::clone(&guest);
        let vm_running = Arc::clone(&vm_running);
        let state_dir = state_dir.clone();
        let changed = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/mac-sync/tick",
            move || {
                let mut scheduler = scheduler
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                scheduler.tick(
                    &state_dir,
                    guest.as_ref(),
                    &*vm_running,
                    started.elapsed().as_secs_f64(),
                    &ProjectRounds,
                )
            },
        )
        .await;
        match changed {
            Ok(true) => changes.notify_one(),
            Ok(false) => {}
            Err(_) => eprintln!("mac sync: a scheduler tick failed"),
        }
    }
}
