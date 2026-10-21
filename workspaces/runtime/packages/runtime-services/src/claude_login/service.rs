//! Per-login locks keep sign-ins ordered without blocking unrelated jobs;
//! account writes and receipt fsync happen off the executor.
use super::{ports::*, receipts::MAX_JOBS, *};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
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
struct Job {
    snapshot: LoginSnapshot,
    verified: bool,
    url: Option<String>,
    commands: Option<mpsc::Sender<Command>>,
}
struct Admission {
    active: bool,
    unknown: bool,
    account: String,
}
impl From<&Job> for Admission {
    fn from(job: &Job) -> Self {
        Self {
            active: job.snapshot.active(),
            unknown: unknown(&job.snapshot),
            account: job.snapshot.account_key().to_owned(),
        }
    }
}
#[derive(Default)]
struct RegistryState {
    jobs: BTreeMap<String, Arc<Mutex<Job>>>,
    admission: HashMap<String, Admission>,
    starting: HashMap<String, Option<String>>,
}
#[derive(Clone, Default)]
struct JobRegistry(Arc<StdMutex<RegistryState>>);
struct StartReservation {
    id: String,
    account: Option<String>,
    registry: JobRegistry,
}
type ReservationResult = Result<(Option<Arc<Mutex<Job>>>, Option<StartReservation>), LoginError>;
impl JobRegistry {
    fn entry(&self, id: &str) -> Result<Arc<Mutex<Job>>, LoginError> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .jobs
            .get(id)
            .cloned()
            .ok_or(LoginError::Unknown)
    }
    fn insert(&self, id: String, job: Job) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.admission.insert(id.clone(), Admission::from(&job));
        state.jobs.insert(id, Arc::new(Mutex::new(job)));
    }
    fn refresh(&self, id: &str, job: &Job) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.admission.insert(id.to_owned(), Admission::from(job));
    }
    fn snapshot(&self) -> Vec<(String, Arc<Mutex<Job>>)> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .jobs
            .iter()
            .map(|(id, job)| (id.clone(), Arc::clone(job)))
            .collect()
    }
    fn reserve(&self, id: &str, account: Option<String>) -> ReservationResult {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = state.jobs.get(id).cloned();
        if existing.is_some() {
            return Ok((existing, None));
        }
        let active = state.admission.values().filter(|job| job.active).count();
        let duplicate = account.as_ref().is_some_and(|account| {
            state.admission.values().any(|admission| {
                (admission.active || admission.unknown) && &admission.account == account
            }) || state
                .starting
                .values()
                .any(|reserved| reserved.as_ref() == Some(account))
        });
        if state.jobs.len() + state.starting.len() >= MAX_JOBS
            || active + state.starting.len() >= MAX_ACTIVE
            || state.starting.contains_key(id)
            || duplicate
        {
            return Err(LoginError::Busy);
        }
        state.starting.insert(id.to_owned(), account.clone());
        Ok((
            None,
            Some(StartReservation {
                id: id.to_owned(),
                account,
                registry: self.clone(),
            }),
        ))
    }
}
impl StartReservation {
    fn finish(self, job: Job) -> Result<(), LoginError> {
        let mut state = self
            .registry
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.starting.get(&self.id) != Some(&self.account) {
            return Err(LoginError::Unavailable);
        }
        state.starting.remove(&self.id);
        state
            .admission
            .insert(self.id.clone(), Admission::from(&job));
        state
            .jobs
            .insert(self.id.clone(), Arc::new(Mutex::new(job)));
        Ok(())
    }
}
impl Drop for StartReservation {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.starting.get(&self.id) == Some(&self.account) {
            state.starting.remove(&self.id);
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
    lease: Option<File>,
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
            jobs: JobRegistry::default(),
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
            service.jobs.insert(
                restored.request_id().into(),
                Job {
                    snapshot: restored,
                    verified: false,
                    url: None,
                    commands: None,
                },
            );
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
        let entry = self.jobs.entry(id)?;
        Ok(public(&*entry.lock().await))
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
        let (existing, reservation) = self.jobs.reserve(&id, account.clone())?;
        if let Some(entry) = existing {
            let job = entry.lock().await;
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
        reservation.ok_or(LoginError::Unavailable)?.finish(Job {
            snapshot,
            verified: false,
            url: None,
            commands: Some(sender),
        })?;
        let service = self.clone();
        let launch = Launch {
            program,
            config,
            profile,
            lease: Some(lease),
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
        // Validate the provider-specific delimiter before persisting the
        // one-shot code intent or writing to the child. A malformed local
        // input is safe to correct; once the pipe write starts it is not.
        super::validate_manual_authorization_code(&code)?;
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
        let (next, write) = job
            .snapshot
            .code_intent(job.url.is_some() && job.commands.is_some())?;
        if !write {
            return Ok(public(&job));
        }
        self.save(&next).await?;
        job.snapshot = next;
        self.jobs.refresh(id, &job);
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
            self.jobs.refresh(id, &job);
            return Ok(public(&job));
        }
        drop(job);
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
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
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
            self.jobs.refresh(id, &job);
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
            self.jobs.refresh(id, &job);
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
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
        if job.snapshot.active() {
            let next = job.snapshot.finish(status, error)?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            self.jobs.refresh(id, &job);
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
                    .as_ref()
                    .ok_or(LoginError::Unavailable)?
                    .try_clone()
                    .map_err(|_| LoginError::Unavailable)?,
            ),
        })
    }
    async fn run(
        self: Arc<Self>,
        id: String,
        mut launch: Launch,
        mut commands: mpsc::Receiver<Command>,
    ) {
        let result = self.run_inner(&id, &mut launch, &mut commands).await;
        if result.is_err()
            && self
                .finish(&id, LoginStatus::Error, Some(FAILED))
                .await
                .is_err()
        {
            self.failed.store(true, Ordering::Release);
        }
        let state = async {
            let entry = self.jobs.entry(&id)?;
            let mut job = entry.lock().await;
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
            self.jobs.refresh(&id, &job);
            Ok::<(), LoginError>(())
        }
        .await;
        if state.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        drop(launch.lease.take());
        // A final receipt write may fail after Ready was installed in memory.
        // Wake readers even when finish() correctly leaves that terminal state intact.
        (self.on_change)();
    }
    async fn run_inner(
        &self,
        id: &str,
        launch: &mut Launch,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<(), LoginError> {
        let snapshot = {
            let entry = self.jobs.entry(id)?;
            let job = entry.lock().await;
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
                            let entry = self.jobs.entry(id)?;
                            let mut job = entry.lock().await;
                            if job.snapshot.active() && job.url.as_deref()!=Some(&url){
                                job.snapshot=job.snapshot.finish(LoginStatus::Pending,None)?;
                                job.url=Some(url);
                                self.jobs.refresh(id, &job);
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
        let (metadata, probe_conclusive) = self.probe(id, launch, commands).await?;
        if !probe_conclusive {
            let entry = self.jobs.entry(id)?;
            let code_submitted = entry.lock().await.snapshot.code_submitted();
            if code_submitted {
                return Err(LoginError::Unavailable);
            }
            self.finish(id, LoginStatus::Error, Some(FAILED)).await?;
            return Ok(());
        }
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
        if !job.snapshot.active() {
            return Ok(());
        }
        if job
            .snapshot
            .validate_completion(&metadata, success)
            .is_err()
        {
            let error = if job.snapshot.code_submitted()
                && probe_conclusive
                && job.snapshot.provider_rejected_completion(&metadata)
            {
                AUTHORIZATION_NOT_CONFIRMED
            } else {
                FAILED
            };
            let next = job.snapshot.finish(LoginStatus::Error, Some(error))?;
            self.save(&next).await?;
            job.snapshot = next;
            job.url = None;
            self.jobs.refresh(id, &job);
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
        // The provider child and status probe are both closed, the account
        // publication is durable, and the Ready receipt is durable. Release
        // the cross-process config lease before exposing Ready to readers.
        drop(launch.lease.take());
        job.snapshot = next;
        job.url = None;
        self.jobs.refresh(id, &job);
        drop(job);
        self.refresh_chats(id).await?;
        (self.on_change)();
        Ok(())
    }
    async fn refresh_chats(&self, id: &str) -> Result<(), LoginError> {
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
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
        self.chats.refresh(job.snapshot.account_key()).await?;
        let mut record = job.snapshot.record().clone();
        record["receipt"]["chatsRefreshed"] = json!(true);
        let next = LoginSnapshot::from_record(record)?;
        self.save(&next).await?;
        job.snapshot = next;
        self.jobs.refresh(id, &job);
        Ok(())
    }
    async fn probe(
        &self,
        id: &str,
        launch: &Launch,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<(AuthMetadata, bool), LoginError> {
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
        let exited_successfully = loop {
            tokio::select! {
                _=&mut deadline=>break None,
                command=commands.recv()=>match command {
                    Some(Command::Code(_, reply))=>{drop(reply.send(Err(LoginError::Unavailable)));},
                    Some(Command::Cancel)|None=>break None,
                },
                event=child.next_event()=>match event?{Some(LoginEvent::Output(bytes))=>append_tail(&mut output,&bytes),Some(LoginEvent::Diagnostic(_))=>{},Some(LoginEvent::Exited(success))=>break Some(success),None=>break None}
            }
        };
        child.close().await?;
        self.record_child(id, None).await?;
        Ok(
            crate::account_driver::claude::auth_status_probe_from_output(
                &output,
                exited_successfully,
            ),
        )
    }
    async fn record_child(
        &self,
        id: &str,
        identity: Option<ChildIdentity>,
    ) -> Result<(), LoginError> {
        let entry = self.jobs.entry(id)?;
        let mut job = entry.lock().await;
        let mut record = job.snapshot.record().clone();
        record["metadata"]["child"] =
            serde_json::to_value(identity).map_err(|_| LoginError::Unavailable)?;
        let next = LoginSnapshot::from_record(record)?;
        self.save(&next).await?;
        job.snapshot = next;
        self.jobs.refresh(id, &job);
        Ok(())
    }
    async fn settle_unknowns(&self) -> Result<(), LoginError> {
        let candidates = self.jobs.snapshot();
        for (id, entry) in candidates {
            let mut job = entry.lock().await;
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
                // saved account identity with its own config directory; never probe
                // credentials during recovery.
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
            self.jobs.refresh(&id, &job);
        }
        Ok(())
    }
    /// A profile without its own config directory is the system Claude Code
    /// login, which the user signs in to outside Studio.
    fn profile_config(&self, profile: &Value) -> Result<PathBuf, LoginError> {
        profile
            .pointer("/claudeOptions/configDir")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or(LoginError::SystemProfile)
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
        // The stored configuration folder is canonical; the state folder may be
        // under a link, such as the macOS `/var`.
        if !snapshot
            .record()
            .pointer("/metadata/configDir")
            .and_then(Value::as_str)
            .is_some_and(|stored| {
                studio_runtime_store::path_identity::same(std::path::Path::new(stored), &expected)
            })
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
        let jobs = self.jobs.snapshot();
        for (_, entry) in jobs {
            let job = entry.lock().await;
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
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/claude_login/service",
        f,
    )
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
    async fn per_login_mutexes_preserve_admission_and_survive_shutdown() {
        fn job(id: usize, account: &str) -> Job {
            let id = format!("00000000-0000-0000-0000-{id:012x}");
            Job {
                snapshot: start_record(&id, account, None, Map::new()).expect("valid test job"),
                verified: false,
                url: None,
                commands: None,
            }
        }

        let registry = JobRegistry::default();
        registry.insert("first".into(), job(1, "first-account"));
        registry.insert("other".into(), job(2, "other-account"));
        let first = registry.entry("first").expect("first job exists");
        let other = registry.entry("other").expect("other job exists");
        let held = first.lock().await;
        let (started, waiting) = oneshot::channel();
        let waiting_entry = Arc::clone(&first);
        let waiter = tokio::spawn(async move {
            assert!(started.send(()).is_ok(), "test waits for waiter start");
            let job = waiting_entry.lock().await;
            job.snapshot.request_id().to_owned()
        });
        waiting.await.expect("same-login waiter started");
        assert!(other.try_lock().is_ok(), "another login is independent");
        assert!(first.try_lock().is_err(), "same-login work is ordered");

        // Dropping the service registry cannot strand an in-flight per-job lock.
        drop(registry);
        drop(held);
        assert_eq!(
            waiter.await.expect("waiter completes"),
            "00000000-0000-0000-0000-000000000001"
        );

        let admission = JobRegistry::default();
        let reserved_id = "00000000-0000-0000-0000-000000000020";
        let duplicate_id = "00000000-0000-0000-0000-000000000021";
        let (_, reservation) = admission
            .reserve(reserved_id, Some("shared-account".into()))
            .expect("first reservation fits");
        assert!(matches!(
            admission.reserve(duplicate_id, Some("shared-account".into())),
            Err(LoginError::Busy)
        ));
        drop(reservation);
        let (_, reservation) = admission
            .reserve(duplicate_id, Some("shared-account".into()))
            .expect("cancelled reservation releases admission");
        reservation
            .expect("new request owns a reservation")
            .finish(job(21, "shared-account"))
            .expect("reservation commits atomically");
        for id in 4..MAX_ACTIVE + 3 {
            admission.insert(format!("login-{id}"), job(id, &format!("account-{id}")));
        }
        assert!(matches!(
            admission.reserve(
                "00000000-0000-0000-0000-000000000099",
                Some("new-account".into())
            ),
            Err(LoginError::Busy)
        ));
    }
}
