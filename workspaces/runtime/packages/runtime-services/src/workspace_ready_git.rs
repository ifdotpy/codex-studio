//! Bounded host Git effects. The durable journal precedes every creation attempt.
//!
//! Like Python `apps/server/src/codex_worktree_creation.py:234`, worktree creation
//! preserves repository hooks, filters and fsmonitor configuration. Only prompts and
//! Git network transports are disabled. Tests configure hooks off in disposable repos.
//! One state root per installation is guaranteed by RuntimeStore's ownership lock;
//! the common-directory mutex serializes workers of that installation.
//! Ready carries a token: the submission transaction MUST validate it before consuming
//! queued input. Provider/account-worker wiring is owned by the account-worker slice.
//! No child runs inside a store transaction. External Git/files can change between the
//! last observation and commit; the common-directory mutex excludes this adapter's own
//! writers, and publication compares actor/reservations/journal again. The opaque Ready
//! token revalidates admission in the submission transaction before exposing cwd. This
//! does not freeze user filesystem edits; uncertain effects stay inspectable and no user
//! worktree is reset or cleaned.
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
    ProvisionIdentity, ReadyToken, WorkspaceReadyRepository, reservations, workspace_generation,
};
use studio_runtime_store::{
    CheckpointsRepository, RuntimeStore, StoreError, WorkspaceSnapshot, path_identity,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
static REPOSITORY_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Invalid workspace record")]
    InvalidRecord,
    /// The named Git step failed; the text says which command and why.
    #[error("Workspace Git observation failed: {0}")]
    Git(String),
    /// The Git budget ran out before or during the named command.
    #[error("Workspace Git deadline expired at {0}; inspect the journal before retrying")]
    Deadline(String),
    #[error("Checkpoint reference already names a different commit")]
    CheckpointRefConflict,
    #[error("Workspace serialization lock is poisoned")]
    Lock,
    #[error("Workspace preparation worker failed")]
    Worker,
}

/// Call on a blocking preparation worker. Queued events belong to the caller and are never mutated.
/// Validate the returned Ready token inside the submission transaction; other admission gates remain.
pub async fn prepare(
    store: Arc<RuntimeStore>,
    agent_id: String,
) -> Result<WorkspaceDecision, WorkspaceError> {
    prepare_with_timeout(store, agent_id, GIT_TIMEOUT).await
}

pub async fn prepare_with_timeout(
    store: Arc<RuntimeStore>,
    agent_id: String,
    timeout: Duration,
) -> Result<WorkspaceDecision, WorkspaceError> {
    match prepare_attempt(Arc::clone(&store), agent_id.clone(), timeout).await {
        Err(WorkspaceError::Deadline(step)) => {
            recovery_required(
                store,
                agent_id,
                format!(
                    "Workspace identity could not be established within the Git deadline ({step})"
                ),
            )
            .await
        }
        Err(WorkspaceError::Git(detail)) => {
            recovery_required(
                store,
                agent_id,
                format!("Workspace identity could not be established: {detail}"),
            )
            .await
        }
        outcome => outcome,
    }
}

async fn recovery_required(
    store: Arc<RuntimeStore>,
    agent_id: String,
    reason: String,
) -> Result<WorkspaceDecision, WorkspaceError> {
    let current = actor(&snapshot(&store, agent_id).await?)?;
    Ok(recovery(
        current.workspace_reservation_id,
        current.cwd,
        reason,
    ))
}

async fn prepare_attempt(
    store: Arc<RuntimeStore>,
    agent_id: String,
    timeout: Duration,
) -> Result<WorkspaceDecision, WorkspaceError> {
    let initial = snapshot(&store, agent_id.clone()).await?;
    let actor = actor(&initial)?;
    // VM paths are guest paths, so they cannot enter Git canonicalization. The
    // native-action owner binds the line; this token admits only the persisted
    // ready association and is rechecked when the turn is submitted.
    if actor.execution_mode.as_deref() == Some("vm") {
        if actor.deleted_at.is_some() || !actor.auto_wake {
            return Ok(WorkspaceDecision::Defer {
                by: crate::workspace_ready::DeferredBy::Actor,
            });
        }
        let value = &initial.agent.record;
        if value.get("layrReady").and_then(Value::as_bool) != Some(true)
            || value
                .get("layrProjectId")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            || value
                .get("layrLine")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Ok(WorkspaceDecision::Defer {
                by: crate::workspace_ready::DeferredBy::Actor,
            });
        }
        let cwd = value
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .ok_or(WorkspaceError::InvalidRecord)?;
        return Ok(WorkspaceDecision::Ready {
            token: ReadyToken::new(value, PathBuf::from(cwd)),
        });
    }
    if actor.image_bound() {
        #[cfg(not(target_os = "macos"))]
        return Ok(image_workspace_refusal_decision());
        #[cfg(target_os = "macos")]
        {
            if actor.deleted_at.is_some() || !actor.auto_wake {
                return Ok(WorkspaceDecision::Defer {
                    by: crate::workspace_ready::DeferredBy::Actor,
                });
            }
            if !actor.image_workspace_ready {
                return Ok(match actor.image_workspace_phase.as_deref() {
                    Some("creating") => WorkspaceDecision::Defer {
                        by: crate::workspace_ready::DeferredBy::Actor,
                    },
                    // Python `prepare_locked` starts the base build and the copy here.
                    // The turn waits for the ready copy instead of running read-only.
                    Some("read_only") => {
                        crate::image_workspace_agents::start(&store, &agent_id);
                        WorkspaceDecision::Defer {
                            by: crate::workspace_ready::DeferredBy::Actor,
                        }
                    }
                    _ => WorkspaceDecision::Refuse(
                        crate::workspace_ready::WorkspaceRefusal::ImageWorkspaceUnsupported,
                    ),
                });
            }
            let read_agent_id = agent_id.clone();
            let mounted = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/workspace_ready_git",
                move || crate::image_workspace::ensure_mounted(&read_agent_id),
            )
            .await
            .map_err(|_| WorkspaceError::Worker)?
            .map_err(|error| WorkspaceError::Git(format!("image workspace mount: {error}")))?;
            let repo = mounted
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or(WorkspaceError::InvalidRecord)?;
            let path = image_turn_cwd(
                &repo,
                initial.agent.record["imageWorkspaceSubpath"].as_str(),
            )?;
            let mount = mounted
                .get("mount")
                .and_then(Value::as_str)
                .ok_or(WorkspaceError::InvalidRecord)?
                .to_owned();
            // Python `prepare_locked`: the record names the mounted directory and
            // mount, and the agent gets the ready notice once.
            let write_id = agent_id.clone();
            let write_path = path.clone();
            let record = store
                .transaction_with(move |tx| {
                    crate::image_workspace_agents::record_mounted(
                        tx,
                        &write_id,
                        &write_path,
                        &mount,
                        now(),
                    )
                })
                .await
                .map_err(|error| match error {
                    crate::event_write::EventWriteError::Store(error) => {
                        WorkspaceError::Store(error)
                    }
                    _ => WorkspaceError::InvalidRecord,
                })?
                .unwrap_or_else(|| initial.agent.record.clone());
            return Ok(WorkspaceDecision::Ready {
                token: ReadyToken::new(&record, path),
            });
        }
    }
    if let Some(refusal) = unsupported(&actor) {
        return Ok(WorkspaceDecision::Refuse(refusal));
    }
    if actor.deleted_at.is_some() || !actor.auto_wake {
        return Ok(WorkspaceDecision::Defer {
            by: crate::workspace_ready::DeferredBy::Actor,
        });
    }
    let journal = provision_journal(&store, provision_id(&actor)).await?;
    let (context, git_deadline) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/workspace_ready_git",
        move || {
            // The Git budget starts with the first Git command. Time spent
            // waiting for a blocking thread or a SQLite reader is not Git time.
            let deadline = Instant::now() + timeout;
            observe(initial, journal, &Git { deadline }).map(|context| (context, deadline))
        },
    )
    .await
    .map_err(|_| WorkspaceError::Worker)??;
    let WorkspaceDecision::Provision(plan) = context.decide() else {
        return Ok(context.decide());
    };
    let lock = repository_lock(&plan.identity.common_dir)?;
    // Overlapping preparations of one repository queue on its lock within the
    // Git deadline; only a lock still held at the deadline defers the turn.
    let lock_deadline = tokio::time::Instant::from_std(git_deadline);
    let Ok(_guard) = tokio::time::timeout_at(lock_deadline, lock.lock()).await else {
        return Ok(WorkspaceDecision::Defer {
            by: crate::workspace_ready::DeferredBy::Workspace,
        });
    };
    // Another worker may have completed while this caller waited for the common repository.
    let observe_agent_id = agent_id.clone();
    let (snapshot, journal) = store
        .read_async("runtime-services/workspace_ready_git", move |connection| {
            let snapshot = reservation_snapshot(connection, &observe_agent_id)?;
            let id = provision_id(&self::actor(&snapshot)?);
            let journal = WorkspaceReadyRepository::journal(connection, &id)?;
            Ok::<_, WorkspaceError>((snapshot, journal))
        })
        .await?;
    let context = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/workspace_ready_git",
        move || {
            observe(
                snapshot,
                journal,
                &Git {
                    deadline: git_deadline,
                },
            )
        },
    )
    .await
    .map_err(|_| WorkspaceError::Worker)??;
    let WorkspaceDecision::Provision(plan) = context.decide() else {
        return Ok(context.decide());
    };
    provision(store, context, plan, git_deadline).await
}

/// The turn directory of an image agent: its mounted `repo` plus the saved
/// subpath, as Python's `prepare_locked` builds it. A subpath that leaves the
/// repository is an invalid record.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn image_turn_cwd(repo: &Path, subpath: Option<&str>) -> Result<PathBuf, WorkspaceError> {
    crate::image_workspace_agents::turn_cwd(repo, subpath).ok_or(WorkspaceError::InvalidRecord)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn image_workspace_refusal_decision() -> WorkspaceDecision {
    WorkspaceDecision::Refuse(crate::workspace_ready::WorkspaceRefusal::ImageWorkspaceUnsupported)
}

async fn provision(
    store: Arc<RuntimeStore>,
    context: Context,
    plan: crate::workspace_ready::WorktreePlan,
    deadline: Instant,
) -> Result<WorkspaceDecision, WorkspaceError> {
    let reserve_snapshot = context.snapshot.clone();
    let reserve_identity = plan.identity.clone();
    if !store
        .transaction(move |tx| {
            WorkspaceReadyRepository::reserve(tx, &reserve_snapshot, &reserve_identity, now())
        })
        .await?
    {
        return Ok(WorkspaceDecision::Defer {
            by: crate::workspace_ready::DeferredBy::Workspace,
        });
    }
    let identity = &plan.identity;
    let effect_plan = identity.clone();
    let effect_kind = plan.effect;
    let effect_context = context;
    let effect = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/workspace_ready_git",
        move || {
            let git = Git { deadline };
            if effect_kind == ProvisionEffect::Create {
                git.create(&effect_plan)?;
            }
            let observed = git.worktree(&effect_plan)?;
            let decision = decide_workspace(
                &effect_context.actor,
                &effect_context.snapshot.agent.record,
                effect_context.cwd.as_ref(),
                &[],
                Some(&effect_plan),
                true,
                &observed,
            );
            if !matches!(decision, WorkspaceDecision::Provision(ref p) if p.effect == ProvisionEffect::Publish)
                || !confined(&effect_plan)
            {
                return Err(WorkspaceError::Git(
                    "the created worktree does not match its provisioning plan".into(),
                ));
            }
            Ok::<_, WorkspaceError>(git.checkpoint(&effect_plan).map_err(|error| error.to_string()))
        },
    )
    .await
    .map_err(|_| WorkspaceError::Worker)?;
    let checkpoint = match effect {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            let failed_identity = identity.clone();
            let failure = error.to_string();
            store
                .transaction(move |tx| {
                    WorkspaceReadyRepository::uncertain(tx, &failed_identity, &failure, now())
                })
                .await?;
            return Ok(recovery(
                Some(identity.id.clone()),
                identity.destination.clone(),
                "Provisioning did not establish a complete worktree",
            ));
        }
    };
    publish_verified(store, identity.clone(), checkpoint, deadline).await
}

fn confined(identity: &ProvisionIdentity) -> bool {
    identity.destination.canonicalize().ok().is_some_and(|p| {
        p == identity.destination
            && p.starts_with(identity.repository.join(".worktrees/codex-agents"))
    }) && identity.cwd.canonicalize().ok().as_ref() == Some(&identity.cwd)
}

/// Observe Git after checkpoint commands, outside the writer transaction. Only bounded
/// state comparisons and publication run while holding the store's writer.
async fn publish_verified(
    store: Arc<RuntimeStore>,
    identity: ProvisionIdentity,
    checkpoint: Result<Value, String>,
    deadline: Instant,
) -> Result<WorkspaceDecision, WorkspaceError> {
    let read_identity = identity.clone();
    let (observed, confined) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/workspace_ready_git",
        move || {
            let git = Git { deadline };
            (git.worktree(&read_identity).ok(), confined(&read_identity))
        },
    )
    .await
    .map_err(|_| WorkspaceError::Worker)?;
    let transaction_identity = identity.clone();
    let token = store.transaction(move |tx| {
        let current = CheckpointsRepository::reservation_snapshot(tx, &transaction_identity.agent)?;
        let valid = current.as_ref().is_some_and(|current| {
            let Ok(actor) = actor(current) else { return false };
            let Some(observed) = &observed else { return false };
            // `decide_workspace` compares resolved paths, as `observe` gives them.
            let cwd = path_identity::resolve(&actor.cwd).unwrap_or_else(|| actor.cwd.clone());
            let decision = decide_workspace(&actor, &current.agent.record,
                Some(&cwd), &reservations(current), Some(&transaction_identity), true, observed);
            confined && matches!(decision, WorkspaceDecision::Provision(ref p) if p.effect == ProvisionEffect::Publish)
        });
        if !valid {
            WorkspaceReadyRepository::uncertain(tx, &transaction_identity, "Workspace evidence changed before publication", now())?;
            return Ok(None);
        }
        let checkpoint = checkpoint.as_ref().map_err(String::as_str);
        if !WorkspaceReadyRepository::publish(tx, &transaction_identity, checkpoint, now())? { return Ok(None); }
        let record = CheckpointsRepository::agent(tx, &transaction_identity.agent)?.expect("published actor");
        Ok(Some(ReadyToken::new(&record.record, transaction_identity.cwd.clone())))
    }).await?;
    match token {
        Some(token) => Ok(WorkspaceDecision::Ready { token }),
        None => Ok(recovery(
            Some(identity.id.clone()),
            identity.destination.clone(),
            "Workspace changed; created worktree remains unattached",
        )),
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
async fn snapshot(store: &RuntimeStore, id: String) -> Result<WorkspaceSnapshot, WorkspaceError> {
    store
        .read_async("runtime-services/workspace_ready_git", move |connection| {
            reservation_snapshot(connection, &id)
        })
        .await
}

fn reservation_snapshot(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<WorkspaceSnapshot, WorkspaceError> {
    CheckpointsRepository::reservation_snapshot(connection, id)?
        .ok_or(WorkspaceError::InvalidRecord)
}

/// The workspace operation journal key of one actor epoch's provisioning.
fn provision_id(actor: &WorkspaceActor) -> String {
    format!("provision:{}:{}", actor.id, actor.epoch)
}

async fn provision_journal(
    store: &RuntimeStore,
    id: String,
) -> Result<Option<Value>, WorkspaceError> {
    Ok(store
        .read_async("runtime-services/workspace_ready_git", move |connection| {
            WorkspaceReadyRepository::journal(connection, &id)
        })
        .await?)
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
    snapshot: WorkspaceSnapshot,
    journal: Option<Value>,
    git: &Git,
) -> Result<Context, WorkspaceError> {
    let actor = actor(&snapshot)?;
    let cwd = actor.cwd.canonicalize().ok().filter(|path| path.is_dir());
    let reservations = reservations(&snapshot);
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
        || !crate::workspace_ready::requires_worktree(&context.actor)
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
            false,
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
    let id = provision_id(&context.actor);
    let repository = canonical(git.run(cwd, &["rev-parse", "--show-toplevel"])?.trim())?;
    let common_dir = git.common(&repository)?;
    let relative = cwd
        .strip_prefix(&repository)
        .map_err(|_| WorkspaceError::InvalidRecord)?;
    let destination = repository
        .join(".worktrees/codex-agents")
        .join(&context.actor.id);
    let plan = if let Some(operation) = journal {
        context.journaled = true;
        serde_json::from_value(operation["plan"].clone())
            .map_err(|_| WorkspaceError::InvalidRecord)?
    } else {
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
            thread_id: source.get("threadId").cloned().unwrap_or_default(),
            cwd: destination.join(relative),
            destination: destination.clone(),
            branch: format!("codex-agent/{}", context.actor.id),
            repository: repository.clone(),
            common_dir,
            base_commit,
        }
    };
    // A stored journal is data, not authority to choose another path or branch.
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

fn repository_lock(path: &Path) -> Result<Arc<tokio::sync::Mutex<()>>, WorkspaceError> {
    let mut locks = REPOSITORY_LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| WorkspaceError::Lock)?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    let lock = locks
        .get(path)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}
struct Git {
    deadline: Instant,
}
impl Git {
    fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
        let step = || format!("git {}", args.join(" "));
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(WorkspaceError::Deadline(step()));
        }
        let mut safe_args = vec!["-c", "protocol.allow=never"];
        // Helpers receive the caller's environment; the runner strips all ambient GIT_*.
        let environment = [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_ALLOW_PROTOCOL", ""),
        ];
        safe_args.extend_from_slice(args);
        let output = bounded_child::run_without_git_environment(bounded_child::Request {
            program: std::ffi::OsStr::new("git"),
            cwd: cwd.as_os_str(),
            args: &safe_args,
            environment: &environment,
            input: None,
            stdout_limit: GIT_OUTPUT_LIMIT,
            stderr_limit: 4096,
            timeout,
        })
        .map_err(|error| match error {
            ChildError::TimedOut => WorkspaceError::Deadline(step()),
            ChildError::Failed(stderr) => WorkspaceError::Git(format!("{}: {stderr}", step())),
            ChildError::Launch(detail) => WorkspaceError::Git(format!(
                "{}: the Git process could not run ({detail})",
                step()
            )),
        })?;
        if output.stdout.truncated {
            return Err(WorkspaceError::Git(format!(
                "{}: output exceeds {GIT_OUTPUT_LIMIT} bytes",
                step()
            )));
        }
        String::from_utf8(output.stdout.bytes)
            .map_err(|_| WorkspaceError::Git(format!("{}: output is not UTF-8", step())))
    }
    fn common(&self, root: &Path) -> Result<PathBuf, WorkspaceError> {
        canonical(
            self.run(
                root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .trim(),
        )
    }
    fn worktree(&self, plan: &ProvisionIdentity) -> Result<GitObservation, WorkspaceError> {
        let common = self.common(&plan.repository)?;
        if common != plan.common_dir {
            return Err(WorkspaceError::Git(format!(
                "the common Git directory changed to {}",
                common.display()
            )));
        }
        self.observe(
            &plan.repository,
            &plan.destination,
            &plan.branch,
            &plan.common_dir,
            true,
        )
    }
    /// `inspect_contents` adds the index, sparse-checkout and status checks that
    /// only an unfinished provisioning needs. A ready worktree is identified by
    /// its registration alone, as in Python, which runs no Git check for it.
    fn observe(
        &self,
        repository: &Path,
        destination: &Path,
        branch: &str,
        common: &Path,
        inspect_contents: bool,
    ) -> Result<GitObservation, WorkspaceError> {
        let listing = self.run(repository, &["worktree", "list", "--porcelain", "-z"])?;
        let mut observation = GitObservation {
            destination_exists: match destination.symlink_metadata() {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(WorkspaceError::Git(format!(
                        "{}: {error}",
                        destination.display()
                    )));
                }
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
                    if !inspect_contents {
                        continue;
                    }
                    let flags = self.run(destination, &["ls-files", "-v", "-z"])?;
                    let safe_index = flags
                        .split('\0')
                        .filter(|s| !s.is_empty())
                        .all(|entry| entry.starts_with("H "));
                    let sparse = self.run(
                        destination,
                        &[
                            "config",
                            "--type=bool",
                            "--default=false",
                            "--get",
                            "core.sparseCheckout",
                        ],
                    )?;
                    observation.clean = safe_index
                        && sparse.trim() == "false"
                        && self
                            .run(
                                destination,
                                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                            )?
                            .is_empty();
                    if observation.clean {
                        self.run(destination, &["diff", "--cached", "--quiet", "HEAD", "--"])?;
                    }
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
                "checkout.workers=16",
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
        let existing = self.run(
            &plan.destination,
            &["for-each-ref", "--format=%(objectname)", &reference],
        )?;
        let expected = if existing.trim().is_empty() {
            "0".repeat(plan.base_commit.len())
        } else {
            plan.base_commit.clone()
        };
        // Exact replay is a compare-and-swap too, not just a potentially stale ref read.
        self.run(
            &plan.destination,
            &["update-ref", &reference, &plan.base_commit, &expected],
        )
        .map_err(|error| match error {
            WorkspaceError::Git(_) => WorkspaceError::CheckpointRefConflict,
            other => other,
        })?;
        Ok(
            json!({"id":id,"agent":plan.agent,"rootId":plan.source.get("rootId"),"label":"Before first turn",
            "tree":tree,"commit":plan.base_commit,"ref":reference,"threadId":plan.thread_id,
            "created":now(),"cwd":plan.cwd}),
        )
    }
}
fn canonical(path: &str) -> Result<PathBuf, WorkspaceError> {
    PathBuf::from(path)
        .canonicalize()
        .map_err(|error| WorkspaceError::Git(format!("{path}: {error}")))
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
