//! One bounded actor per native sign-in; account writes and receipt fsync happen
//! off the executor, and no provider I/O occurs inside an account/store lock.
use super::{ports::*, receipts::MAX_JOBS, *};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    ffi::OsString,
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, mpsc, oneshot};
const MAX_ACTIVE: usize = 16;
const OUTPUT_LIMIT: usize = 32768;
const LOGIN_DEADLINE: Duration = Duration::from_secs(600);
const PROBE_DEADLINE: Duration = Duration::from_secs(15);
const COMMAND_DEADLINE: Duration = Duration::from_secs(15);

/// A failed pre-launch admission may remove only the empty directory it created.
struct NewConfig(Option<PathBuf>);
impl Drop for NewConfig {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            drop(std::fs::remove_dir(path));
        }
    }
}
pub struct LoginConfig {
    pub directory: PathBuf,
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
}
#[derive(Clone)]
struct Job {
    snapshot: LoginSnapshot,
    verified: bool,
    url: Option<String>,
    commands: Option<mpsc::Sender<Command>>,
}
enum RegistryCommand {
    Acquire(String, oneshot::Sender<u64>),
    Load(
        String,
        u64,
        oneshot::Sender<Result<Option<Job>, LoginError>>,
    ),
    Update(String, u64, Job, oneshot::Sender<Result<(), LoginError>>),
    Release(String, u64),
    Reserve(
        String,
        Option<String>,
        oneshot::Sender<Result<Option<Job>, LoginError>>,
    ),
    FinishReservation(
        String,
        Option<String>,
        Job,
        oneshot::Sender<Result<(), LoginError>>,
    ),
    CancelReservation(String),
    Insert(String, Job, oneshot::Sender<()>),
    Snapshot(oneshot::Sender<Vec<(String, Job)>>),
}
#[derive(Clone)]
struct JobRegistry(mpsc::Sender<RegistryCommand>);
struct JobLease {
    id: String,
    token: u64,
    registry: mpsc::Sender<RegistryCommand>,
}
struct StartReservation {
    id: String,
    account: Option<String>,
    registry: mpsc::Sender<RegistryCommand>,
    active: bool,
}

impl JobRegistry {
    fn new() -> Self {
        let (sender, mut receiver) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut jobs = BTreeMap::<String, Job>::new();
            let mut active = HashMap::<String, u64>::new();
            let mut waiters = HashMap::<String, VecDeque<(u64, oneshot::Sender<u64>)>>::new();
            let mut starting = HashMap::<String, Option<String>>::new();
            let mut next_token = 1_u64;
            while let Some(command) = receiver.recv().await {
                match command {
                    RegistryCommand::Acquire(id, reply) => {
                        if active.contains_key(&id) {
                            waiters
                                .entry(id)
                                .or_default()
                                .push_back((next_token, reply));
                            next_token = next_token.wrapping_add(1).max(1);
                        } else {
                            let token = next_token;
                            next_token = next_token.wrapping_add(1).max(1);
                            active.insert(id, token);
                            let _ = reply.send(token);
                        }
                    }
                    RegistryCommand::Load(id, token, reply) => {
                        if active.get(&id) == Some(&token) {
                            let _ = reply.send(Ok(jobs.get(&id).cloned()));
                        } else {
                            let _ = reply.send(Err(LoginError::Unavailable));
                        }
                    }
                    RegistryCommand::Update(id, token, job, reply) => {
                        if active.get(&id) == Some(&token) {
                            jobs.insert(id, job);
                            let _ = reply.send(Ok(()));
                        } else {
                            let _ = reply.send(Err(LoginError::Unavailable));
                        }
                    }
                    RegistryCommand::Release(id, token) => {
                        if active.get(&id) == Some(&token) {
                            let mut granted = false;
                            if let Some(queue) = waiters.get_mut(&id) {
                                while let Some((next, reply)) = queue.pop_front() {
                                    if !reply.is_closed() {
                                        active.insert(id.clone(), next);
                                        let _ = reply.send(next);
                                        granted = true;
                                        break;
                                    }
                                }
                                if queue.is_empty() {
                                    waiters.remove(&id);
                                }
                            }
                            if !granted {
                                active.remove(&id);
                            }
                        }
                    }
                    RegistryCommand::Reserve(id, account, reply) => {
                        let existing = jobs.get(&id).cloned();
                        let busy = jobs.len() + starting.len() >= MAX_JOBS
                            || jobs.values().filter(|j| j.snapshot.active()).count()
                                + starting.len()
                                >= MAX_ACTIVE
                            || starting.contains_key(&id)
                            || jobs.values().any(|j| {
                                (j.snapshot.active() || unknown(&j.snapshot))
                                    && account.as_deref() == Some(j.snapshot.account_key())
                            })
                            || (account.is_some()
                                && starting.values().any(|reserved| reserved == &account));
                        let result = if existing.is_some() {
                            Ok(existing)
                        } else if busy {
                            Err(LoginError::Busy)
                        } else {
                            starting.insert(id, account);
                            Ok(None)
                        };
                        let _ = reply.send(result);
                    }
                    RegistryCommand::FinishReservation(id, account, job, reply) => {
                        if starting.get(&id) == Some(&account) {
                            starting.remove(&id);
                            jobs.insert(id, job);
                            let _ = reply.send(Ok(()));
                        } else {
                            let _ = reply.send(Err(LoginError::Unavailable));
                        }
                    }
                    RegistryCommand::CancelReservation(id) => {
                        starting.remove(&id);
                    }
                    RegistryCommand::Insert(id, job, reply) => {
                        jobs.insert(id, job);
                        let _ = reply.send(());
                    }
                    RegistryCommand::Snapshot(reply) => {
                        drop(
                            reply.send(
                                jobs.iter()
                                    .map(|(id, job)| (id.clone(), job.clone()))
                                    .collect(),
                            ),
                        );
                    }
                }
            }
        });
        Self(sender)
    }
    async fn acquire(&self, id: &str) -> Result<JobLease, LoginError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(RegistryCommand::Acquire(id.to_owned(), reply))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        let token = receive.await.map_err(|_| LoginError::Unavailable)?;
        Ok(JobLease {
            id: id.to_owned(),
            token,
            registry: self.0.clone(),
        })
    }
    async fn reserve(
        &self,
        id: &str,
        account: Option<String>,
    ) -> Result<(Option<Job>, StartReservation), LoginError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(RegistryCommand::Reserve(
                id.to_owned(),
                account.clone(),
                reply,
            ))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        let existing = receive.await.map_err(|_| LoginError::Unavailable)??;
        let active = existing.is_none();
        Ok((
            existing,
            StartReservation {
                id: id.to_owned(),
                account,
                registry: self.0.clone(),
                active,
            },
        ))
    }
    async fn insert(&self, id: String, job: Job) -> Result<(), LoginError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(RegistryCommand::Insert(id, job, reply))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        receive.await.map_err(|_| LoginError::Unavailable)
    }
    async fn snapshot(&self) -> Result<Vec<(String, Job)>, LoginError> {
        let (reply, receive) = oneshot::channel();
        self.0
            .send(RegistryCommand::Snapshot(reply))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        receive.await.map_err(|_| LoginError::Unavailable)
    }
}
impl JobLease {
    async fn load(&self) -> Result<Job, LoginError> {
        let (reply, receive) = oneshot::channel();
        self.registry
            .send(RegistryCommand::Load(self.id.clone(), self.token, reply))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        receive
            .await
            .map_err(|_| LoginError::Unavailable)??
            .ok_or(LoginError::Unknown)
    }
    async fn update(&self, job: Job) -> Result<(), LoginError> {
        let (reply, receive) = oneshot::channel();
        self.registry
            .send(RegistryCommand::Update(
                self.id.clone(),
                self.token,
                job,
                reply,
            ))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        receive.await.map_err(|_| LoginError::Unavailable)?
    }
}
impl Drop for JobLease {
    fn drop(&mut self) {
        let command = RegistryCommand::Release(self.id.clone(), self.token);
        if let Err(mpsc::error::TrySendError::Full(command)) = self.registry.try_send(command) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let sender = self.registry.clone();
                drop(runtime.spawn(async move {
                    drop(sender.send(command).await);
                }));
            }
        }
    }
}
impl StartReservation {
    async fn finish(mut self, job: Job) -> Result<(), LoginError> {
        let (reply, receive) = oneshot::channel();
        self.registry
            .send(RegistryCommand::FinishReservation(
                self.id.clone(),
                self.account.clone(),
                job,
                reply,
            ))
            .await
            .map_err(|_| LoginError::Unavailable)?;
        receive.await.map_err(|_| LoginError::Unavailable)??;
        self.active = false;
        Ok(())
    }
}
impl Drop for StartReservation {
    fn drop(&mut self) {
        if self.active {
            let command = RegistryCommand::CancelReservation(self.id.clone());
            if let Err(mpsc::error::TrySendError::Full(command)) = self.registry.try_send(command)
                && let Ok(runtime) = tokio::runtime::Handle::try_current()
            {
                let sender = self.registry.clone();
                drop(runtime.spawn(async move {
                    drop(sender.send(command).await);
                }));
            }
        }
    }
}
enum Command {
    Code(Vec<u8>, oneshot::Sender<Result<(), LoginError>>),
    Cancel,
}
struct Launch {
    program: PathBuf,
    config: PathBuf,
    profile: Option<Value>,
    lease: File,
}
pub struct LoginService {
    config: LoginConfig,
    accounts: Arc<dyn LoginAccounts>,
    launcher: Arc<dyn LoginLauncher>,
    receipts: Arc<dyn LoginReceipts>,
    chats: Arc<dyn LoginChats>,
    jobs: JobRegistry,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    failed: AtomicBool,
    on_change: Arc<dyn Fn() + Send + Sync>,
}
impl LoginService {
    pub async fn open(
        config: LoginConfig,
        accounts: Arc<dyn LoginAccounts>,
        launcher: Arc<dyn LoginLauncher>,
        receipts: Arc<dyn LoginReceipts>,
        chats: Arc<dyn LoginChats>,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Arc<Self>, LoginError> {
        let reader = receipts.clone();
        let loaded = blocking(move || reader.load()).await?;
        let service = Arc::new(Self {
            config,
            accounts,
            launcher,
            receipts,
            chats,
            jobs: JobRegistry::new(),
            tasks: Mutex::new(Vec::new()),
            failed: AtomicBool::new(false),
            on_change,
        });
        for snapshot in loaded {
            let account = if snapshot.adding() {
                let config = snapshot
                    .record()
                    .pointer("/metadata/configDir")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LoginError::InvalidRecord("configDir".into()))?
                    .to_owned();
                let accounts = service.accounts.clone();
                blocking(move || accounts.find_config(&config)).await?
            } else {
                None
            };
            let restored = snapshot.restore(account.as_ref())?;
            let restored = if snapshot.active() && account.is_none() {
                let mut record = restored.record().clone();
                record["metadata"]["outcomeUnknown"] = json!(true);
                record["receipt"]["error"] = json!(
                    "Sign-in outcome is unknown after restart. Waiting for its config lease. Cancel this request to clear only its hold without retrying."
                );
                LoginSnapshot::from_record(record)?
            } else {
                restored
            };
            if restored.record() != snapshot.record() {
                service.save(&restored).await?;
            }
            let restored = service.cleanup(&restored).await?;
            service
                .jobs
                .insert(
                    restored.request_id().into(),
                    Job {
                        snapshot: restored,
                        verified: false,
                        url: None,
                        commands: None,
                    },
                )
                .await?;
        }
        Ok(service)
    }
    fn available(&self) -> Result<(), LoginError> {
        if self.failed.load(Ordering::Acquire) {
            Err(LoginError::Persistence)
        } else {
            Ok(())
        }
    }
    async fn save(&self, snapshot: &LoginSnapshot) -> Result<(), LoginError> {
        let receipts = self.receipts.clone();
        let record = snapshot.clone();
        let result = blocking(move || receipts.save(&record)).await;
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }
    pub async fn status(&self, id: &str) -> Result<Value, LoginError> {
        request_id(id)?;
        self.settle_unknowns().await?;
        self.refresh_chats(id).await?;
        let lease = self.jobs.acquire(id).await?;
        Ok(public(&lease.load().await?))
    }
    pub async fn start(
        self: &Arc<Self>,
        id: String,
        account: Option<String>,
        email: Option<String>,
        label: Option<String>,
    ) -> Result<Value, LoginError> {
        self.available()?;
        self.settle_unknowns().await?;
        request_id(&id)?;
        let email = email.map(|e| e.trim().to_owned());
        let label = label
            .map(|v| v.trim().to_owned())
            .unwrap_or_else(|| "Claude Code".into());
        let signature = account
            .is_none()
            .then(|| crate::event_write::request_signature(&json!({"email":email,"label":label})));
        let (existing, reservation) = self.jobs.reserve(&id, account.clone()).await?;
        if existing.is_some() {
            let lease = self.jobs.acquire(&id).await?;
            let job = lease.load().await?;
            job.snapshot
                .replay(account.as_deref(), signature.as_deref())?;
            return Ok(public(&job));
        }
        let profile = if let Some(key) = &account {
            let accounts = self.accounts.clone();
            let key = key.clone();
            Some(blocking(move || accounts.profile(&key)).await?)
        } else {
            None
        };
        let desired = profile
            .as_ref()
            .and_then(|p| p.pointer("/claudeOptions/binaryPath"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.program.clone());
        let resolved = self.launcher.resolve(&desired, &self.config.environment)?;
        let mut new_config = NewConfig(None);
        let (config, program, mut metadata, receipt_email) = if let Some(profile) = &profile {
            let saved_email = profile
                .get("email")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or(LoginError::Unavailable)?;
            if profile.get("accountId") != Some(&json!(format!("claude:{saved_email}"))) {
                return Err(LoginError::Unavailable);
            }
            let config = self.profile_config(profile)?;
            (
                config,
                resolved.clone(),
                Map::new(),
                Some(saved_email.to_owned()),
            )
        } else {
            if label.is_empty()
                || label.chars().count() > 32
                || email.as_ref().is_some_and(|e| {
                    !regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").is_ok_and(|r| r.is_match(e))
                })
            {
                return Err(LoginError::Conflict);
            }
            let config = self.config.directory.join(format!("config-{id}"));
            let path = config.clone();
            blocking(move || {
                std::fs::create_dir(&path).map_err(|_| LoginError::Conflict)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| LoginError::Unavailable)?;
                }
                Ok(())
            })
            .await?;
            new_config.0 = Some(config.clone());
            (config.clone(),resolved.clone(),json!({"flow":"add","requestHash":signature,"emailHint":email,"label":label,"configDir":config,"binaryPath":resolved,"codeSubmitted":false,"cleanupDone":false}).as_object().cloned().ok_or(LoginError::Unavailable)?,email.clone())
        };
        let config = self.canonical_config(config, true).await?;
        let lease_path = self.config.directory.join(format!(
            "{}.lock",
            studio_runtime_store::sha256_hex(config.to_string_lossy().as_bytes())
        ));
        let lease = blocking(move || {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lease_path)
                .map_err(|_| LoginError::Unavailable)?;
            file.try_lock().map_err(|_| LoginError::Busy)?;
            Ok(file)
        })
        .await?;
        metadata.insert("configDir".into(), json!(config));
        metadata.insert("inheritedLease".into(), json!(true));
        // Additive metadata keeps the existing receipt representation.
        let key = account.unwrap_or_else(|| format!("pending-{id}"));
        let snapshot = start_record(
            &id,
            &key,
            receipt_email.as_deref(),
            std::mem::take(&mut metadata),
        )?;
        self.save(&snapshot).await?;
        new_config.0 = None;
        let (sender, receiver) = mpsc::channel(4);
        let result = snapshot.receipt();
        reservation
            .finish(Job {
                snapshot,
                verified: false,
                url: None,
                commands: Some(sender),
            })
            .await?;
        let service = self.clone();
        let launch = Launch {
            program,
            config,
            profile,
            lease,
        };
        let task = tokio::spawn(async move {
            service.run(id, launch, receiver).await;
        });
        let mut tasks = self.tasks.lock().await;
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
        (self.on_change)();
        Ok(result)
    }
    pub async fn code(&self, id: &str, code: String) -> Result<Value, LoginError> {
        self.available()?;
        request_id(id)?;
        validate_code(&code)?;
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        let (next, write) = job
            .snapshot
            .code_intent(job.url.is_some() && job.commands.is_some())?;
        if !write {
            return Ok(public(&job));
        }
        self.save(&next).await?;
        job.snapshot = next;
        lease.update(job.clone()).await?;
        let sender = job.commands.clone().ok_or(LoginError::WaitForLink)?;
        let (reply, receipt) = oneshot::channel();
        let bytes = (code + "\n").into_bytes();
        if sender.try_send(Command::Code(bytes, reply)).is_err() {
            let next = job
                .snapshot
                .finish(LoginStatus::Error, Some(CODE_UNKNOWN))?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            lease.update(job.clone()).await?;
            return Ok(public(&job));
        }
        drop(lease);
        if !matches!(
            tokio::time::timeout(COMMAND_DEADLINE, receipt).await,
            Ok(Ok(Ok(())))
        ) {
            self.finish(id, LoginStatus::Error, Some(CODE_UNKNOWN))
                .await?;
            // A failed write can finish the actor before this redundant cancel.
            // A closed receiver must not poison the durable unknown receipt.
            if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send(Command::Cancel) {
                self.failed.store(true, Ordering::Release);
            }
        }
        self.status(id).await
    }
    /// Cancel an active child, or explicitly clear exactly one recovered unknown
    /// receipt. Clearing retains its config and leaves lease admission enforced.
    pub async fn cancel(&self, id: &str) -> Result<Value, LoginError> {
        self.available()?;
        request_id(id)?;
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        if unknown(&job.snapshot) && job.commands.is_none() {
            // Explicit cancellation acknowledges only this receipt. It does not prove
            // the old child exited, delete its config, or resend any input.
            let mut record = job.snapshot.record().clone();
            record["metadata"]["outcomeUnknown"] = json!(false);
            record["metadata"]["operatorCleared"] = json!(true);
            record["receipt"]["status"] = json!("cancelled");
            record["receipt"]["error"] = json!(
                "Unknown sign-in hold cleared by cancellation. Its configuration was retained; no input was retried."
            );
            let next = LoginSnapshot::from_record(record)?;
            self.save(&next).await?;
            job.snapshot = next;
            lease.update(job.clone()).await?;
            (self.on_change)();
        }
        if job.snapshot.active() {
            let next = job.snapshot.finish(LoginStatus::Cancelled, None)?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            if let Some(sender) = &job.commands
                && sender.try_send(Command::Cancel).is_err()
            {
                self.failed.store(true, Ordering::Release);
                return Err(LoginError::Unavailable);
            }
            lease.update(job.clone()).await?;
            (self.on_change)();
        }
        Ok(public(&job))
    }
    async fn finish(
        &self,
        id: &str,
        status: LoginStatus,
        error: Option<&str>,
    ) -> Result<(), LoginError> {
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        if job.snapshot.active() {
            let next = job.snapshot.finish(status, error)?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            lease.update(job.clone()).await?;
            (self.on_change)();
        }
        Ok(())
    }
    fn command(
        &self,
        launch: &Launch,
        args: Vec<OsString>,
        timeout: Duration,
    ) -> Result<LoginCommand, LoginError> {
        let mut environment = crate::account_driver::claude::subscription_environment(
            self.config.environment.clone(),
        );
        environment.insert("CLAUDE_CONFIG_DIR".into(), launch.config.as_os_str().into());
        environment.insert("BROWSER".into(), "/usr/bin/false".into());
        environment.insert("NO_COLOR".into(), "1".into());
        Ok(LoginCommand {
            program: launch.program.clone(),
            args,
            cwd: self.config.cwd.clone(),
            environment,
            timeout,
            inherited_lease: Some(
                launch
                    .lease
                    .try_clone()
                    .map_err(|_| LoginError::Unavailable)?,
            ),
        })
    }
    async fn run(
        self: Arc<Self>,
        id: String,
        launch: Launch,
        mut commands: mpsc::Receiver<Command>,
    ) {
        let result = self.run_inner(&id, &launch, &mut commands).await;
        if result.is_err()
            && self
                .finish(&id, LoginStatus::Error, Some(FAILED))
                .await
                .is_err()
        {
            self.failed.store(true, Ordering::Release);
        }
        let state = async {
            let lease = self.jobs.acquire(&id).await?;
            let mut job = lease.load().await?;
            if result.is_err() {
                let mut record = job.snapshot.record().clone();
                record["metadata"]["outcomeUnknown"] = json!(true);
                if let Ok(next) = LoginSnapshot::from_record(record) {
                    if self.save(&next).await.is_err() {
                        self.failed.store(true, Ordering::Release);
                    }
                    job.snapshot = next;
                }
            }
            job.commands = None;
            match self.cleanup(&job.snapshot).await {
                Ok(next) => job.snapshot = next,
                Err(_) => self.failed.store(true, Ordering::Release),
            }
            lease.update(job).await
        }
        .await;
        if state.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        drop(launch.lease);
        // A final receipt write may fail after Ready was installed in memory.
        // Wake readers even when finish() correctly leaves that terminal state intact.
        (self.on_change)();
    }
    async fn run_inner(
        &self,
        id: &str,
        launch: &Launch,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<(), LoginError> {
        let snapshot = {
            let lease = self.jobs.acquire(id).await?;
            let job = lease.load().await?;
            if !job.snapshot.active() {
                return Ok(());
            }
            job.snapshot.clone()
        };
        let mut args: Vec<OsString> = ["auth", "login", "--claudeai"]
            .into_iter()
            .map(Into::into)
            .collect();
        if let Some(email) = snapshot
            .record()
            .pointer(if snapshot.adding() {
                "/metadata/emailHint"
            } else {
                "/receipt/email"
            })
            .and_then(Value::as_str)
        {
            args.extend(["--email".into(), email.into()]);
        }
        let mut child = self
            .launcher
            .spawn(self.command(launch, args, LOGIN_DEADLINE)?)
            .await?;
        self.record_child(id, child.identity().await?).await?;
        let end = tokio::time::sleep(LOGIN_DEADLINE);
        tokio::pin!(end);
        let mut output = Vec::new();
        let outcome = loop {
            tokio::select! {
                _=&mut end=>{self.finish(id,LoginStatus::Error,Some(TIMED_OUT)).await?;break None;},
                command=commands.recv()=>match command{
                    Some(Command::Code(bytes,reply))=>{let result=child.write(bytes).await;let failed=result.is_err();drop(reply.send(result));if failed{self.finish(id,LoginStatus::Error,Some(CODE_UNKNOWN)).await?;break None;}},
                    Some(Command::Cancel)|None=>break None,
                },
                event=child.next_event()=>match event?{
                    Some(LoginEvent::Output(bytes)|LoginEvent::Diagnostic(bytes))=>{
                        append_tail(&mut output,&bytes);
                        if let Some(url)=find_url(&output){
                            let lease = self.jobs.acquire(id).await?;
                            let mut job = lease.load().await?;
                            if job.snapshot.active() && job.url.as_deref()!=Some(&url){
                                job.snapshot=job.snapshot.finish(LoginStatus::Pending,None)?;
                                job.url=Some(url);
                                lease.update(job).await?;
                                (self.on_change)();
                            }
                        }
                    },
                    Some(LoginEvent::Exited(success))=>break Some(success),None=>break None,
                }
            }
        };
        child.close().await?;
        self.record_child(id, None).await?;
        let Some(success) = outcome else {
            self.finish(id, LoginStatus::Error, Some(FAILED)).await?;
            return Ok(());
        };
        let metadata = self.probe(id, launch, commands).await?;
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        if !job.snapshot.active() {
            return Ok(());
        }
        if job
            .snapshot
            .validate_completion(&metadata, success)
            .is_err()
        {
            let next = job.snapshot.finish(LoginStatus::Error, Some(FAILED))?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            lease.update(job).await?;
            (self.on_change)();
            return Ok(());
        }
        let accounts = self.accounts.clone();
        let record = job.snapshot.clone();
        let profile = launch.profile.clone();
        let home = launch.config.clone();
        let binary = launch.program.clone();
        let account = blocking(move || {
            if let Some(profile) = profile {
                accounts.refresh(record.account_key(), &profile, &metadata)
            } else {
                let label = record
                    .record()
                    .pointer("/metadata/label")
                    .and_then(Value::as_str)
                    .ok_or(LoginError::Unavailable)?;
                let options = json!({"binaryPath":binary,"configDir":home,"customModels":[]})
                    .as_object()
                    .cloned()
                    .ok_or(LoginError::Unavailable)?;
                accounts.register(options, label, &home, &metadata)
            }
        })
        .await?;
        if account.get("status") != Some(&json!("ready")) {
            return Err(LoginError::Unavailable);
        }
        let next = job.snapshot.ready(&account, job.snapshot.adding())?;
        job.verified = true;
        self.save(&next).await?;
        job.snapshot = next;
        job.url = None;
        lease.update(job).await?;
        drop(lease);
        self.refresh_chats(id).await?;
        (self.on_change)();
        Ok(())
    }
    async fn refresh_chats(&self, id: &str) -> Result<(), LoginError> {
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        if job.snapshot.record().pointer("/receipt/status") != Some(&json!("ready"))
            || job.snapshot.record().pointer("/receipt/chatsRefreshed") == Some(&json!(true))
        {
            return Ok(());
        }
        let accounts = self.accounts.clone();
        let key = job.snapshot.account_key().to_owned();
        let verified = job.verified;
        let account = blocking(move || {
            if verified {
                accounts.profile(&key)
            } else {
                accounts.revalidate(&key)
            }
        })
        .await?;
        let email = job
            .snapshot
            .record()
            .pointer("/receipt/email")
            .and_then(Value::as_str)
            .ok_or(LoginError::Unavailable)?;
        if account.get("status") != Some(&json!("ready"))
            || account.get("accountId") != Some(&json!(format!("claude:{email}")))
        {
            return Ok(());
        }
        let chats = self.chats.clone();
        let key = job.snapshot.account_key().to_owned();
        blocking(move || chats.refresh(&key)).await?;
        let mut record = job.snapshot.record().clone();
        record["receipt"]["chatsRefreshed"] = json!(true);
        let next = LoginSnapshot::from_record(record)?;
        self.save(&next).await?;
        job.snapshot = next;
        lease.update(job).await?;
        Ok(())
    }
    async fn probe(
        &self,
        id: &str,
        launch: &Launch,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<AuthMetadata, LoginError> {
        let mut child = self
            .launcher
            .spawn(
                self.command(
                    launch,
                    ["auth", "status", "--json"]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    PROBE_DEADLINE,
                )?,
            )
            .await?;
        self.record_child(id, child.identity().await?).await?;
        let mut output = Vec::new();
        let deadline = tokio::time::sleep(PROBE_DEADLINE);
        tokio::pin!(deadline);
        let success = loop {
            tokio::select! {
                _=&mut deadline=>break false,
                command=commands.recv()=>match command {
                    Some(Command::Code(_, reply))=>{drop(reply.send(Err(LoginError::Unavailable)));},
                    Some(Command::Cancel)|None=>break false,
                },
                event=child.next_event()=>match event?{Some(LoginEvent::Output(bytes))=>append_tail(&mut output,&bytes),Some(LoginEvent::Diagnostic(_))=>{},Some(LoginEvent::Exited(success))=>break success,None=>break false}
            }
        };
        child.close().await?;
        self.record_child(id, None).await?;
        Ok(crate::account_driver::claude::auth_metadata_from_output(
            &output, success,
        ))
    }
    async fn record_child(
        &self,
        id: &str,
        identity: Option<ChildIdentity>,
    ) -> Result<(), LoginError> {
        let lease = self.jobs.acquire(id).await?;
        let mut job = lease.load().await?;
        let mut record = job.snapshot.record().clone();
        record["metadata"]["child"] =
            serde_json::to_value(identity).map_err(|_| LoginError::Unavailable)?;
        let next = LoginSnapshot::from_record(record)?;
        self.save(&next).await?;
        job.snapshot = next;
        lease.update(job).await?;
        Ok(())
    }
    async fn settle_unknowns(&self) -> Result<(), LoginError> {
        let candidates = self.jobs.snapshot().await?;
        for (id, _candidate) in candidates
            .into_iter()
            .filter(|(_, job)| unknown(&job.snapshot))
        {
            let lease = self.jobs.acquire(&id).await?;
            let mut job = lease.load().await?;
            if !unknown(&job.snapshot) {
                continue;
            }
            let metadata = job.snapshot.record()["metadata"].clone();
            if let Some(value) = metadata.get("child").filter(|v| !v.is_null()) {
                let identity: ChildIdentity =
                    serde_json::from_value(value.clone()).map_err(|_| LoginError::Unavailable)?;
                if !self.launcher.gone(identity).await.unwrap_or(false) {
                    continue;
                }
            }
            let config = if let Some(config) = metadata.get("configDir").and_then(Value::as_str) {
                PathBuf::from(config)
            } else if !job.snapshot.adding() {
                // Python reauthentication receipts omit configDir, but Python children
                // inherit this same canonical-config lease. Resolve only a matching
                // saved account identity; never probe credentials during recovery.
                let accounts = self.accounts.clone();
                let key = job.snapshot.account_key().to_owned();
                let Ok(profile) = blocking(move || accounts.profile(&key)).await else {
                    continue;
                };
                let Some(email) = job
                    .snapshot
                    .record()
                    .pointer("/receipt/email")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                if profile.get("accountId") != Some(&json!(format!("claude:{email}"))) {
                    continue;
                }
                let Ok(config) = self.profile_config(&profile) else {
                    continue;
                };
                config
            } else {
                continue;
            };
            let Ok(config) = self.canonical_config(config, false).await else {
                continue;
            };
            let path = self.config.directory.join(format!(
                "{}.lock",
                studio_runtime_store::sha256_hex(config.to_string_lossy().as_bytes())
            ));
            let config_lease = blocking(move || {
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|_| LoginError::Unavailable)?;
                file.try_lock().map_err(|_| LoginError::Busy)?;
                Ok(file)
            })
            .await;
            let Ok(_config_lease) = config_lease else {
                continue;
            };
            let mut record = job.snapshot.record().clone();
            record["metadata"]["outcomeUnknown"] = json!(false);
            record["receipt"]["error"] =
                json!("The previous sign-in config lease is free. Start a new sign-in request.");
            let next = LoginSnapshot::from_record(record)?;
            self.save(&next).await?;
            job.snapshot = self.cleanup(&next).await?;
            lease.update(job).await?;
        }
        Ok(())
    }
    fn profile_config(&self, profile: &Value) -> Result<PathBuf, LoginError> {
        profile
            .pointer("/claudeOptions/configDir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| {
                self.config
                    .environment
                    .get(std::ffi::OsStr::new("CLAUDE_CONFIG_DIR"))
                    .map(PathBuf::from)
            })
            .or_else(|| {
                self.config
                    .environment
                    .get(std::ffi::OsStr::new("HOME"))
                    .map(|v| PathBuf::from(v).join(".claude"))
            })
            .ok_or(LoginError::Unavailable)
    }
    async fn canonical_config(
        &self,
        supplied: PathBuf,
        create: bool,
    ) -> Result<PathBuf, LoginError> {
        let home = self
            .config
            .environment
            .get(std::ffi::OsStr::new("HOME"))
            .map(PathBuf::from);
        let cwd = self.config.cwd.clone();
        blocking(move || {
            let expanded = match supplied.to_str() {
                Some("~") => home.clone().ok_or(LoginError::Unavailable)?,
                Some(v) if v.starts_with("~/") => {
                    home.ok_or(LoginError::Unavailable)?.join(&v[2..])
                }
                _ => supplied,
            };
            let absolute = if expanded.is_absolute() {
                expanded
            } else {
                cwd.join(expanded)
            };
            if create && !absolute.exists() {
                std::fs::create_dir_all(&absolute).map_err(|_| LoginError::Unavailable)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| LoginError::Unavailable)?;
                }
            }
            absolute.canonicalize().map_err(|_| LoginError::Unavailable)
        })
        .await
    }
    async fn cleanup(&self, snapshot: &LoginSnapshot) -> Result<LoginSnapshot, LoginError> {
        if !snapshot.adding()
            || snapshot.active()
            || unknown(snapshot)
            || snapshot.record().pointer("/metadata/operatorCleared") == Some(&json!(true))
            || snapshot.record().pointer("/receipt/status") == Some(&json!("ready"))
            || snapshot.record().pointer("/metadata/cleanupDone") == Some(&json!(true))
        {
            return Ok(snapshot.clone());
        }
        let expected = self
            .config
            .directory
            .join(format!("config-{}", snapshot.request_id()));
        if snapshot
            .record()
            .pointer("/metadata/configDir")
            .and_then(Value::as_str)
            != expected.to_str()
        {
            return Ok(snapshot.clone());
        }
        // Account publication can commit before the final receipt. Preserve and
        // adopt that publication before removing an abandoned add configuration.
        let accounts = self.accounts.clone();
        let config = expected.to_string_lossy().into_owned();
        if let Some(account) = blocking(move || accounts.find_config(&config)).await? {
            let next = snapshot.restore(Some(&account))?;
            self.save(&next).await?;
            return Ok(next);
        }
        blocking(move || match std::fs::symlink_metadata(&expected) {
            Ok(meta) if !meta.file_type().is_symlink() => {
                std::fs::remove_dir_all(expected).map_err(|_| LoginError::Unavailable)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(LoginError::Unavailable),
        })
        .await?;
        let mut record = snapshot.record().clone();
        record["metadata"]["cleanupDone"] = json!(true);
        let next = LoginSnapshot::from_record(record)?;
        self.save(&next).await?;
        Ok(next)
    }
    pub async fn shutdown(&self) -> Result<(), LoginError> {
        let jobs = self.jobs.snapshot().await?;
        for (id, _) in jobs {
            let lease = self.jobs.acquire(&id).await?;
            let job = lease.load().await?;
            if let Some(sender) = &job.commands
                && sender.try_send(Command::Cancel).is_err()
            {
                self.failed.store(true, Ordering::Release);
            }
        }
        for task in self.tasks.lock().await.drain(..) {
            tokio::time::timeout(COMMAND_DEADLINE, task)
                .await
                .map_err(|_| LoginError::Unavailable)?
                .map_err(|_| LoginError::Unavailable)?;
        }
        Ok(())
    }
}
fn public(job: &Job) -> Value {
    let mut receipt = job.snapshot.receipt();
    if let Some(url) = &job.url {
        receipt["status"] = json!("pending");
        receipt["verificationUrl"] = json!(url);
    }
    receipt
}
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, LoginError> + Send + 'static,
) -> Result<T, LoginError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| LoginError::Unavailable)?
}
fn append_tail(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(bytes);
    if output.len() > OUTPUT_LIMIT {
        output.drain(..output.len() - OUTPUT_LIMIT);
    }
}
fn find_url(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    static PATTERN: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let pattern = PATTERN
        .get_or_init(|| regex::Regex::new(r"https://[^\s\x1b]+[\s\x1b]"))
        .as_ref()
        .ok()?;
    pattern
        .find_iter(&text)
        .map(|m| {
            m.as_str()
                .trim_end_matches(|c: char| c.is_whitespace() || c == '\x1b')
        })
        .find(|url| verification_url(url))
        .map(str::to_owned)
}

fn unknown(snapshot: &LoginSnapshot) -> bool {
    snapshot.record().pointer("/metadata/outcomeUnknown") == Some(&json!(true))
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[tokio::test]
    async fn owner_runs_independent_requests_and_closes_queued_replies_on_shutdown() {
        let registry = JobRegistry::new();
        let (held_reply, held) = oneshot::channel();
        registry
            .0
            .send(RegistryCommand::Acquire("same".into(), held_reply))
            .await
            .expect("owner accepts first lease");
        let _held_token = held.await.expect("owner grants first lease");

        let (queued_reply, queued) = oneshot::channel();
        registry
            .0
            .send(RegistryCommand::Acquire("same".into(), queued_reply))
            .await
            .expect("owner accepts queued lease");
        let (barrier_reply, barrier) = oneshot::channel();
        registry
            .0
            .send(RegistryCommand::Snapshot(barrier_reply))
            .await
            .expect("owner accepts barrier");
        barrier.await.expect("owner crossed queue barrier");

        let (other_reply, other) = oneshot::channel();
        registry
            .0
            .send(RegistryCommand::Acquire("other".into(), other_reply))
            .await
            .expect("owner accepts independent lease");
        assert!(other.await.is_ok(), "another login proceeds concurrently");

        // Keep the first key leased and drop every sender. The actor ends and
        // drops its queued reply, so waiters receive an error instead of hanging.
        drop(registry);
        assert!(queued.await.is_err(), "shutdown closes pending requests");
    }
}
