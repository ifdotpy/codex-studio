//! Bounded host Git effects. The durable journal precedes every creation attempt.
use crate::bounded_child::{self, ChildError};
use crate::workspace_ready::{
    GitObservation, ProvisionEffect, Reservation, WorkspaceActor, WorkspaceDecision,
    decide_workspace, recovery, unsupported,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_store::workspace_ready::{
    ProvisionIdentity, WorkspaceReadyRepository, workspace_generation,
};
use studio_runtime_store::{CheckpointsRepository, RuntimeStore, StoreError, WorkspaceSnapshot};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
static REPOSITORY_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Invalid workspace record")]
    InvalidRecord,
    #[error("Workspace Git observation failed")]
    Git,
    #[error("Workspace Git deadline expired; inspect the journal before retrying")]
    Deadline,
    #[error("Workspace serialization lock is poisoned")]
    Lock,
}

/// Call on a blocking preparation worker. Queued events belong to the caller and are never mutated.
/// Re-run immediately before provider submission; this does not replace other admission gates.
pub fn prepare(store: &RuntimeStore, agent_id: &str) -> Result<WorkspaceDecision, WorkspaceError> {
    prepare_with_timeout(store, agent_id, GIT_TIMEOUT)
}

pub fn prepare_with_timeout(
    store: &RuntimeStore,
    agent_id: &str,
    timeout: Duration,
) -> Result<WorkspaceDecision, WorkspaceError> {
    match prepare_attempt(store, agent_id, timeout) {
        Err(WorkspaceError::Git | WorkspaceError::Deadline) => {
            let current = actor(&snapshot(store, agent_id)?)?;
            Ok(recovery(
                current.workspace_reservation_id,
                current.cwd,
                "Workspace identity could not be established within the Git deadline",
            ))
        }
        outcome => outcome,
    }
}

fn prepare_attempt(
    store: &RuntimeStore,
    agent_id: &str,
    timeout: Duration,
) -> Result<WorkspaceDecision, WorkspaceError> {
    let initial = snapshot(store, agent_id)?;
    let actor = actor(&initial)?;
    if let Some(refusal) = unsupported(&actor) {
        return Ok(WorkspaceDecision::Refuse(refusal));
    }
    if actor.deleted_at.is_some() || !actor.auto_wake {
        return Ok(WorkspaceDecision::Defer { reservation: None });
    }
    let git = Git {
        deadline: Instant::now() + timeout,
    };
    let mut context = observe(store, initial, &git)?;
    let decision = context.decide();
    let WorkspaceDecision::Provision(plan) = decision else {
        return Ok(decision);
    };
    let lock = repository_lock(&plan.identity.common_dir)?;
    let _guard = match lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Ok(WorkspaceDecision::Defer {
                reservation: Some(plan.identity.id),
            });
        }
        Err(std::sync::TryLockError::Poisoned(_)) => return Err(WorkspaceError::Lock),
    };
    // Another worker may have completed while this caller waited for the common repository.
    context = observe(store, snapshot(store, agent_id)?, &git)?;
    let WorkspaceDecision::Provision(plan) = context.decide() else {
        return Ok(context.decide());
    };
    provision(store, &context, plan, &git)
}

fn provision(
    store: &RuntimeStore,
    context: &Context,
    plan: crate::workspace_ready::WorktreePlan,
    git: &Git,
) -> Result<WorkspaceDecision, WorkspaceError> {
    if !store.transaction(|tx| {
        WorkspaceReadyRepository::reserve(tx, &context.snapshot, &plan.identity, now())
    })? {
        return Ok(WorkspaceDecision::Defer {
            reservation: Some(plan.identity.id),
        });
    }
    let identity = &plan.identity;
    let effect = (|| {
        if plan.effect == ProvisionEffect::Create {
            git.create(identity)?;
        }
        let observed = git.worktree(identity)?;
        let record = &context.snapshot.agent.record;
        let decision = decide_workspace(
            &context.actor,
            record,
            context.cwd.as_ref(),
            &[],
            Some(identity),
            true,
            &observed,
        );
        if !matches!(decision, WorkspaceDecision::Provision(ref p) if p.effect == ProvisionEffect::Publish)
            || identity.cwd.canonicalize().ok().as_ref() != Some(&identity.cwd)
        {
            return Err(WorkspaceError::Git);
        }
        Ok(())
    })();
    if let Err(error) = effect {
        store.transaction(|tx| {
            WorkspaceReadyRepository::uncertain(tx, identity, &error.to_string(), now())
        })?;
        return Ok(recovery(
            Some(identity.id.clone()),
            identity.destination.clone(),
            "Provisioning did not establish a complete worktree",
        ));
    }
    let checkpoint = git.checkpoint(identity);
    let checkpoint_error = checkpoint
        .as_ref()
        .err()
        .map(|error| format!("Checkpoint skipped: {error}"));
    let outcome = checkpoint
        .as_ref()
        .map_err(|_| checkpoint_error.as_deref().unwrap_or("Checkpoint skipped"));
    let attached =
        store.transaction(|tx| WorkspaceReadyRepository::publish(tx, identity, outcome, now()))?;
    if attached {
        Ok(WorkspaceDecision::Ready {
            cwd: identity.cwd.clone(),
        })
    } else {
        Ok(recovery(
            Some(identity.id.clone()),
            identity.destination.clone(),
            "Actor changed; created worktree remains unattached",
        ))
    }
}

struct Context {
    snapshot: WorkspaceSnapshot,
    actor: WorkspaceActor,
    cwd: Option<PathBuf>,
    reservations: Vec<Reservation>,
    plan: Option<ProvisionIdentity>,
    journaled: bool,
    observed: GitObservation,
}
impl Context {
    fn decide(&self) -> WorkspaceDecision {
        decide_workspace(
            &self.actor,
            &self.snapshot.agent.record,
            self.cwd.as_ref(),
            &self.reservations,
            self.plan.as_ref(),
            self.journaled,
            &self.observed,
        )
    }
}
fn snapshot(store: &RuntimeStore, id: &str) -> Result<WorkspaceSnapshot, WorkspaceError> {
    CheckpointsRepository::workspace_snapshot(&store.read()?, id)?
        .ok_or(WorkspaceError::InvalidRecord)
}
fn actor(snapshot: &WorkspaceSnapshot) -> Result<WorkspaceActor, WorkspaceError> {
    let actor: WorkspaceActor = serde_json::from_value(snapshot.agent.record.clone())
        .map_err(|_| WorkspaceError::InvalidRecord)?;
    if actor.id != snapshot.agent.id {
        return Err(WorkspaceError::InvalidRecord);
    }
    Ok(actor)
}
fn observe(
    store: &RuntimeStore,
    snapshot: WorkspaceSnapshot,
    git: &Git,
) -> Result<Context, WorkspaceError> {
    let actor = actor(&snapshot)?;
    let cwd = actor.cwd.canonicalize().ok().filter(|path| path.is_dir());
    let mut reservations = Vec::new();
    for other in &snapshot.agents {
        if let Some(kind) = other
            .record
            .get("workspaceOperation")
            .and_then(Value::as_str)
        {
            let path = other
                .record
                .get("cwd")
                .and_then(Value::as_str)
                .ok_or(WorkspaceError::InvalidRecord)?;
            let path = Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path));
            let id = other
                .record
                .get("workspaceReservationId")
                .and_then(Value::as_str);
            let op = snapshot
                .operations
                .iter()
                .find(|op| Some(op.id.as_str()) == id)
                .or_else(|| {
                    snapshot
                        .operations
                        .iter()
                        .find(|op| op.record["agent"] == other.id)
                });
            reservations.push(Reservation {
                agent: other.id.clone(),
                cwd: path,
                operation: kind.to_owned(),
                id: id.map(str::to_owned).or_else(|| op.map(|o| o.id.clone())),
                phase: op
                    .and_then(|o| o.record["phase"].as_str())
                    .map(str::to_owned),
            });
        }
    }
    // Marker loss must not hide an unresolved durable restore, branch, or provision operation.
    for operation in &snapshot.operations {
        if reservations
            .iter()
            .any(|r| r.id.as_deref() == Some(&operation.id))
        {
            continue;
        }
        let record = &operation.record;
        let path = record
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or(WorkspaceError::InvalidRecord)?;
        reservations.push(Reservation {
            agent: record["agent"]
                .as_str()
                .ok_or(WorkspaceError::InvalidRecord)?
                .to_owned(),
            cwd: Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(path)),
            operation: record["kind"]
                .as_str()
                .ok_or(WorkspaceError::InvalidRecord)?
                .to_owned(),
            id: Some(operation.id.clone()),
            phase: record["phase"].as_str().map(str::to_owned),
        });
    }
    let mut context = Context {
        snapshot,
        actor,
        cwd,
        reservations,
        plan: None,
        journaled: false,
        observed: GitObservation::default(),
    };
    if unsupported(&context.actor).is_some()
        || !context.actor.auto_wake
        || context.actor.deleted_at.is_some()
        || (!context.actor.worktree && context.actor.workspace_mode.as_deref() != Some("worktree"))
    {
        return Ok(context);
    }
    let Some(cwd) = &context.cwd else {
        return Ok(context);
    };
    if context.actor.worktree_ready {
        let root = PathBuf::from(git.run(cwd, &["rev-parse", "--show-toplevel"])?.trim());
        let common = git.common(&root)?;
        context.observed = git.observe(
            &root,
            &root,
            context.actor.branch.as_deref().unwrap_or(""),
            &common,
        )?;
        return Ok(context);
    }
    if !matches!(
        Path::new(&context.actor.id)
            .components()
            .collect::<Vec<_>>()
            .as_slice(),
        [Component::Normal(_)]
    ) {
        return Err(WorkspaceError::InvalidRecord);
    }
    let id = format!("provision:{}:{}", context.actor.id, context.actor.epoch);
    let journal = WorkspaceReadyRepository::journal(&store.read()?, &id)?;
    let plan = if let Some(operation) = journal {
        context.journaled = true;
        serde_json::from_value(operation["plan"].clone())
            .map_err(|_| WorkspaceError::InvalidRecord)?
    } else {
        let repository = PathBuf::from(git.run(cwd, &["rev-parse", "--show-toplevel"])?.trim())
            .canonicalize()
            .map_err(|_| WorkspaceError::Git)?;
        let common_dir = git.common(&repository)?;
        let relative = cwd
            .strip_prefix(&repository)
            .map_err(|_| WorkspaceError::InvalidRecord)?;
        let destination = repository
            .join(".worktrees/codex-agents")
            .join(&context.actor.id);
        let source = &context.snapshot.agent.record;
        let base = source
            .get("workerBaseCommit")
            .and_then(Value::as_str)
            .unwrap_or("HEAD");
        let base_commit = git
            .run(
                &repository,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{base}^{{commit}}"),
                ],
            )?
            .trim()
            .to_owned();
        ProvisionIdentity {
            id,
            agent: context.actor.id.clone(),
            source: workspace_generation(source),
            cwd: destination.join(relative),
            destination,
            branch: format!("codex-agent/{}", context.actor.id),
            repository,
            common_dir,
            base_commit,
        }
    };
    // A stored journal is data, not authority to choose another path or branch.
    let repository = PathBuf::from(git.run(cwd, &["rev-parse", "--show-toplevel"])?.trim())
        .canonicalize()
        .map_err(|_| WorkspaceError::Git)?;
    let relative = cwd
        .strip_prefix(&repository)
        .map_err(|_| WorkspaceError::InvalidRecord)?;
    let destination = repository
        .join(".worktrees/codex-agents")
        .join(&context.actor.id);
    if plan.repository != repository
        || plan.destination != destination
        || plan.cwd != destination.join(relative)
        || plan.branch != format!("codex-agent/{}", context.actor.id)
        || plan.id != format!("provision:{}:{}", context.actor.id, context.actor.epoch)
        || !matches!(plan.base_commit.len(), 40 | 64)
        || !plan.base_commit.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(WorkspaceError::InvalidRecord);
    }
    for ancestor in destination
        .ancestors()
        .skip(1)
        .take_while(|p| *p != repository)
    {
        if ancestor.symlink_metadata().is_ok()
            && ancestor.canonicalize().ok().as_deref() != Some(ancestor)
        {
            return Err(WorkspaceError::InvalidRecord);
        }
    }
    context.observed = git.worktree(&plan)?;
    context.plan = Some(plan);
    Ok(context)
}

fn repository_lock(path: &Path) -> Result<Arc<Mutex<()>>, WorkspaceError> {
    let mut locks = REPOSITORY_LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| WorkspaceError::Lock)?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = locks
        .get(path)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| Arc::new(Mutex::new(())));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}
struct Git {
    deadline: Instant,
}
impl Git {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(WorkspaceError::Deadline);
        }
        let mut safe_args = vec![
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.allow=never",
        ];
        safe_args.extend_from_slice(args);
        let output = bounded_child::run(bounded_child::Request {
            program: std::ffi::OsStr::new("git"),
            cwd: cwd.as_os_str(),
            args: &safe_args,
            environment: &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0")],
            input: None,
            stdout_limit: GIT_OUTPUT_LIMIT,
            stderr_limit: 4096,
            timeout,
        })
        .map_err(|error| match error {
            ChildError::TimedOut => WorkspaceError::Deadline,
            _ => WorkspaceError::Git,
        })?;
        if output.stdout.truncated {
            return Err(WorkspaceError::Git);
        }
        String::from_utf8(output.stdout.bytes).map_err(|_| WorkspaceError::Git)
    }
    fn common(&self, root: &Path) -> Result<PathBuf, WorkspaceError> {
        PathBuf::from(
            self.run(
                root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .trim(),
        )
        .canonicalize()
        .map_err(|_| WorkspaceError::Git)
    }
    fn worktree(&self, plan: &ProvisionIdentity) -> Result<GitObservation, WorkspaceError> {
        if self.common(&plan.repository)? != plan.common_dir {
            return Err(WorkspaceError::Git);
        }
        self.observe(
            &plan.repository,
            &plan.destination,
            &plan.branch,
            &plan.common_dir,
        )
    }
    fn observe(
        &self,
        repository: &Path,
        destination: &Path,
        branch: &str,
        common: &Path,
    ) -> Result<GitObservation, WorkspaceError> {
        let listing = self.run(repository, &["worktree", "list", "--porcelain", "-z"])?;
        let mut observation = GitObservation {
            destination_exists: match destination.symlink_metadata() {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => return Err(WorkspaceError::Git),
            },
            directory: destination.canonicalize().ok().filter(|p| p.is_dir()),
            branch_exists: !self
                .run(
                    repository,
                    &[
                        "for-each-ref",
                        "--format=%(refname)",
                        &format!("refs/heads/{branch}"),
                    ],
                )?
                .is_empty(),
            ..GitObservation::default()
        };
        for block in listing.split("\0\0") {
            let fields = block
                .split('\0')
                .filter_map(|line| line.split_once(' '))
                .collect::<BTreeMap<_, _>>();
            if fields
                .get("worktree")
                .is_some_and(|p| Path::new(p) == destination)
            {
                observation.registered = true;
                observation.branch = fields
                    .get("branch")
                    .and_then(|b| b.strip_prefix("refs/heads/"))
                    .map(str::to_owned);
                observation.head = fields.get("HEAD").map(|s| (*s).to_owned());
                if observation.directory.as_deref() == Some(destination) {
                    observation.common_dir = Some(self.common(destination)?);
                    observation.clean = self
                        .run(
                            destination,
                            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                        )?
                        .is_empty();
                }
            }
        }
        if observation
            .common_dir
            .as_deref()
            .is_some_and(|p| p != common)
        {
            observation.registered = false;
        }
        Ok(observation)
    }
    fn create(&self, plan: &ProvisionIdentity) -> Result<(), WorkspaceError> {
        self.run(
            &plan.repository,
            &[
                "-c",
                "core.sparseCheckout=false",
                "worktree",
                "add",
                "-b",
                &plan.branch,
                path_string(&plan.destination)?,
                &plan.base_commit,
            ],
        )?;
        Ok(())
    }
    fn checkpoint(&self, plan: &ProvisionIdentity) -> Result<Value, WorkspaceError> {
        let tree = self
            .run(&plan.destination, &["rev-parse", "HEAD^{tree}"])?
            .trim()
            .to_owned();
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, plan.id.as_bytes()).to_string();
        let reference = format!("refs/codex-agents/checkpoints/{}/{id}", plan.agent);
        self.run(
            &plan.destination,
            &["update-ref", &reference, &plan.base_commit],
        )?;
        Ok(
            json!({"id":id,"agent":plan.agent,"rootId":plan.source.get("rootId"),"label":"Before first turn",
            "tree":tree,"commit":plan.base_commit,"ref":reference,"threadId":plan.source.get("threadId"),
            "turnId":null,"created":now(),"cwd":plan.cwd,"historyParent":null,"historyDelta":[],"historyBoundary":null}),
        )
    }
}
fn path_string(path: &Path) -> Result<&str, WorkspaceError> {
    path.to_str().ok_or(WorkspaceError::InvalidRecord)
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/workspace_ready/effects.rs"]
mod tests;
