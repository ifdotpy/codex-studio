//! Native media intents, immutable request identities and one physical player.
use crate::{
    delivery::{StoreCommand, StoreSender},
    protocol::{PlaybackReceipt, PlaybackStatus},
};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};
use tokio::sync::{mpsc::WeakSender, watch};

/// Retained requests per native media service. Never evict a live owner's receipt.
const MAX_PLAYBACK_RECEIPTS: usize = 256;
/// Maximum native media progress subscriptions. Unit: subscriptions.
const MAX_PLAYBACK_WATCHERS: usize = 256;

/// Compare the captured Store without retaining it in the receipt ledger.
pub(super) fn same_owner(owner: &WeakSender<StoreCommand>, store: &StoreSender) -> bool {
    owner
        .upgrade()
        .is_some_and(|owner| owner.same_channel(store))
}

pub(super) trait PlaybackPlayer {
    fn stop(self);
}
struct Record {
    owner: WeakSender<StoreCommand>,
    binding: String,
    ack: PlaybackReceipt,
    current: PlaybackReceipt,
}
struct Watcher {
    owner: WeakSender<StoreCommand>,
    request: Option<String>,
    output: watch::Sender<PlaybackReceipt>,
}
#[derive(Default)]
struct Ledger {
    records: VecDeque<Record>,
    pending: Option<String>,
    active: Option<String>,
    watchers: Vec<Watcher>,
}
fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(Mutex::default)
}
fn stopped_service() -> String {
    "The native playback service stopped.".into()
}
impl Ledger {
    fn prune(&mut self) {
        self.records
            .retain(|record| record.owner.strong_count() > 0);
        if self
            .pending
            .as_deref()
            .is_some_and(|token| self.index(token).is_none())
        {
            self.pending = None;
            self.busy();
        }
        self.watchers.retain(|watcher| {
            watcher.owner.strong_count() > 0 && watcher.output.receiver_count() > 0
        });
    }
    fn index(&self, token: &str) -> Option<usize> {
        self.records
            .iter()
            .position(|record| record.current.playback_id.as_deref() == Some(token))
    }
    fn publish(&mut self, index: usize) {
        self.watchers.retain(|watcher| {
            watcher.owner.strong_count() > 0 && watcher.output.receiver_count() > 0
        });
        let record = &self.records[index];
        for watcher in &self.watchers {
            if watcher
                .owner
                .upgrade()
                .is_some_and(|owner| same_owner(&record.owner, &owner))
                && watcher
                    .request
                    .as_deref()
                    .is_none_or(|id| id == record.current.request_id)
            {
                watcher.output.send_replace(record.current.clone());
            }
        }
    }
    fn status(
        &mut self,
        token: &str,
        status: PlaybackStatus,
        error: Option<String>,
        duration: Option<f64>,
    ) {
        let Some(index) = self.index(token) else {
            return;
        };
        if self.records[index].current.status.terminal() {
            return;
        }
        self.records[index].current.status = status;
        self.records[index].current.error = error;
        if duration.is_some() {
            self.records[index].current.duration = duration;
        }
        self.publish(index);
    }
    fn busy(&self) {
        if let Ok(mut service) = super::service().lock() {
            service.playback_active = self.pending.is_some() || self.active.is_some();
        }
    }
}

pub(super) struct Reservation {
    pub receipt: PlaybackReceipt,
    pub fresh: bool,
}
pub(super) fn reserve(
    owner: &StoreSender,
    request: &str,
    binding: &str,
) -> Result<Reservation, String> {
    if request.is_empty() {
        return Err("A native playback request identity is required.".into());
    }
    let mut state = ledger().lock().map_err(|_| stopped_service())?;
    state.prune();
    if let Some(record) = state
        .records
        .iter()
        .find(|record| same_owner(&record.owner, owner) && record.current.request_id == request)
    {
        if record.binding != binding {
            return Err("The native playback request belongs to another file.".into());
        }
        return Ok(Reservation {
            receipt: record.ack.clone(),
            fresh: false,
        });
    }
    if state.records.len() >= MAX_PLAYBACK_RECEIPTS {
        return Err("Close this server connection before opening more media previews.".into());
    }
    // A poisoned device owner can never authorize a new effect.
    super::service()
        .lock()
        .map_err(|_| stopped_service())?
        .playback_active = true;
    let token = match crate::file_save::uuid() {
        Ok(token) => token,
        Err(error) => {
            state.busy();
            return Err(error);
        }
    };
    if let Some(previous) = state.pending.take() {
        state.status(&previous, PlaybackStatus::Stopped, None, None);
    }
    let receipt = PlaybackReceipt {
        request_id: request.into(),
        status: PlaybackStatus::Preparing,
        playback_id: Some(token.clone()),
        preview_handle: binding.strip_prefix("preview:").map(str::to_owned),
        error: None,
        duration: None,
    };
    state.records.push_back(Record {
        owner: owner.downgrade(),
        binding: binding.into(),
        ack: receipt.clone(),
        current: receipt.clone(),
    });
    state.pending = Some(token);
    let index = state.records.len() - 1;
    state.publish(index);
    state.busy();
    Ok(Reservation {
        receipt,
        fresh: true,
    })
}
pub(super) fn state(owner: &StoreSender, request: &str) -> Result<PlaybackReceipt, String> {
    let state = ledger().lock().map_err(|_| stopped_service())?;
    Ok(state
        .records
        .iter()
        .find(|record| same_owner(&record.owner, owner) && record.current.request_id == request)
        .map(|record| record.current.clone())
        .unwrap_or_else(|| PlaybackReceipt::unknown(request)))
}
pub(super) fn receipt(owner: &StoreSender, token: &str) -> Result<PlaybackReceipt, String> {
    let state = ledger().lock().map_err(|_| stopped_service())?;
    state
        .records
        .iter()
        .find(|record| {
            same_owner(&record.owner, owner) && record.current.playback_id.as_deref() == Some(token)
        })
        .map(|record| record.current.clone())
        .ok_or("The native playback request is not confirmed.".into())
}
pub(super) fn cancel_all_pending() {
    if let Ok(mut state) = ledger().lock()
        && let Some(token) = state.pending.take()
    {
        state.status(&token, PlaybackStatus::Stopped, None, None);
        state.busy();
    }
}
pub(super) fn preparing(owner: &StoreSender, token: &str) -> bool {
    ledger().lock().is_ok_and(|state| {
        state.pending.as_deref() == Some(token)
            && state.index(token).is_some_and(|index| {
                same_owner(&state.records[index].owner, owner)
                    && state.records[index].current.status == PlaybackStatus::Preparing
            })
    })
}
pub(super) fn preparing_token(token: &str) -> bool {
    ledger()
        .lock()
        .is_ok_and(|state| state.pending.as_deref() == Some(token))
}
pub(super) fn cancel_native_pending(token: &str) {
    if let Ok(mut state) = ledger().lock()
        && state.pending.as_deref() == Some(token)
    {
        state.pending = None;
        state.status(token, PlaybackStatus::Stopped, None, None);
        state.busy();
    }
}
pub(super) fn fail(owner: &StoreSender, token: &str, error: String) -> Result<(), String> {
    let mut state = ledger().lock().map_err(|_| stopped_service())?;
    if state.pending.as_deref() == Some(token)
        && state
            .index(token)
            .is_some_and(|index| same_owner(&state.records[index].owner, owner))
    {
        state.pending = None;
        state.status(token, PlaybackStatus::Failed, Some(error), None);
        state.busy();
    }
    Ok(())
}
pub(super) fn cancel_pending(owner: &StoreSender, token: Option<&str>) -> Result<(), String> {
    let mut state = ledger().lock().map_err(|_| stopped_service())?;
    if let Some(pending) = state.pending.clone()
        && token.is_none_or(|token| token == pending)
        && state
            .index(&pending)
            .is_some_and(|index| same_owner(&state.records[index].owner, owner))
    {
        state.pending = None;
        state.status(&pending, PlaybackStatus::Stopped, None, None);
        state.busy();
    }
    Ok(())
}
pub(super) fn watch(
    owner: &StoreSender,
    request: Option<&str>,
) -> Result<watch::Receiver<PlaybackReceipt>, String> {
    let mut state = ledger().lock().map_err(|_| stopped_service())?;
    state.prune();
    if state.watchers.len() >= MAX_PLAYBACK_WATCHERS {
        return Err("Too many native playback views are open.".into());
    }
    let initial = request
        .and_then(|request| {
            state.records.iter().find(|record| {
                same_owner(&record.owner, owner) && record.current.request_id == request
            })
        })
        .map(|record| record.current.clone())
        .unwrap_or_else(|| PlaybackReceipt::unknown(request.unwrap_or("")));
    let (output, input) = watch::channel(initial);
    // Registration and the initial snapshot share the transition lock.
    state.watchers.push(Watcher {
        owner: owner.downgrade(),
        request: request.map(str::to_owned),
        output,
    });
    Ok(input)
}

pub(super) struct Active<P> {
    owner: StoreSender,
    token: String,
    player: P,
}
pub(super) struct Playback<P> {
    pub(super) current: Option<Active<P>>,
}
impl<P: PlaybackPlayer> Playback<P> {
    pub(super) const fn new() -> Self {
        Self { current: None }
    }
    pub(super) fn with_player<T>(&self, token: &str, read: impl FnOnce(&P) -> T) -> Option<T> {
        self.current
            .as_ref()
            .filter(|active| active.token == token)
            .map(|active| read(&active.player))
    }
    pub(super) fn finish(
        &mut self,
        owner: StoreSender,
        token: &str,
        create: impl FnOnce() -> Result<(P, Option<f64>), String>,
    ) -> Result<PlaybackReceipt, String> {
        let mut state = ledger().lock().map_err(|_| stopped_service())?;
        let index = state
            .index(token)
            .ok_or("The native playback request is not confirmed.")?;
        if !same_owner(&state.records[index].owner, &owner) {
            return Err("The native playback belongs to another server context.".into());
        }
        if state.pending.as_deref() != Some(token)
            || state.records[index].current.status != PlaybackStatus::Preparing
        {
            return Ok(state.records[index].current.clone());
        }
        if let Some(active) = self.current.take() {
            active.player.stop();
            state.status(&active.token, PlaybackStatus::Stopped, None, None);
        }
        state.active = None;
        state.pending = None;
        match create() {
            Ok((player, duration)) => {
                self.current = Some(Active {
                    owner,
                    token: token.into(),
                    player,
                });
                state.active = Some(token.into());
                state.status(token, PlaybackStatus::Playing, None, duration);
            }
            Err(error) => state.status(token, PlaybackStatus::Failed, Some(error), None),
        }
        state.busy();
        Ok(state.records[index].current.clone())
    }
    pub(super) fn stop(&mut self, owner: &StoreSender) -> Result<(), String> {
        self.stop_token(owner, None)
    }
    pub(super) fn stop_token(
        &mut self,
        owner: &StoreSender,
        token: Option<&str>,
    ) -> Result<(), String> {
        let mut state = ledger().lock().map_err(|_| stopped_service())?;
        if let Some(pending) = state.pending.clone()
            && token.is_none_or(|token| token == pending)
            && state
                .index(&pending)
                .is_some_and(|index| same_owner(&state.records[index].owner, owner))
        {
            state.pending = None;
            state.status(&pending, PlaybackStatus::Stopped, None, None);
        }
        if self.current.as_ref().is_some_and(|active| {
            active.owner.same_channel(owner) && token.is_none_or(|token| token == active.token)
        }) {
            let active = self.current.take().unwrap();
            active.player.stop();
            if state.active.as_deref() == Some(&active.token) {
                state.active = None;
            }
            state.status(&active.token, PlaybackStatus::Stopped, None, None);
        }
        state.busy();
        Ok(())
    }
    pub(super) fn fail_token(&mut self, token: &str, error: String) {
        let Ok(mut state) = ledger().lock() else {
            return;
        };
        if self
            .current
            .as_ref()
            .is_some_and(|active| active.token == token)
        {
            self.current.take().unwrap().player.stop();
            state.active = None;
            state.status(token, PlaybackStatus::Failed, Some(error), None);
            state.busy();
        }
    }
    pub(super) fn stop_native_token(&mut self, token: &str) {
        let owner = self
            .current
            .as_ref()
            .filter(|active| active.token == token)
            .map(|active| active.owner.clone());
        if let Some(owner) = owner {
            let _ = self.stop_token(&owner, Some(token));
        }
    }
    pub(super) fn stop_all(&mut self) {
        let Ok(mut state) = ledger().lock() else {
            return;
        };
        if let Some(pending) = state.pending.take() {
            state.status(&pending, PlaybackStatus::Stopped, None, None);
        }
        if let Some(active) = self.current.take() {
            active.player.stop();
            state.status(&active.token, PlaybackStatus::Stopped, None, None);
        }
        state.active = None;
        state.busy();
    }
}
