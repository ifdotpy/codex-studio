//! Restricted local media assets. Native objects remain on the UI thread.
use super::{mac, playback};
use crate::{
    delivery::{StoreCommand, StoreSender},
    protocol::PlaybackReceipt,
};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, ProtocolObject},
    sel,
};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowDelegate, NSWindowStyleMask};
use objc2_av_foundation::{
    AVAssetReferenceRestrictions, AVAsynchronousKeyValueLoading, AVKeyValueStatus, AVPlayer,
    AVPlayerItem, AVPlayerItemStatus, AVURLAsset, AVURLAssetReferenceRestrictionsKey,
};
use objc2_av_kit::AVPlayerView;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSKeyValueChangeKey, NSKeyValueObservingOptions,
    NSNotification, NSNumber, NSObject, NSObjectNSKeyValueObserverRegistration, NSObjectProtocol,
    NSPoint, NSRect, NSSize, NSString, NSURL,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::sync::{mpsc::WeakSender, oneshot};

/// Readiness deadline for one already staged native media asset. Unit: seconds.
const MEDIA_READY_TIMEOUT: Duration = Duration::from_secs(15);
fn item_tokens() -> &'static Mutex<HashMap<usize, String>> {
    static ITEMS: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();
    ITEMS.get_or_init(Mutex::default)
}
fn on_ui(work: impl FnOnce() + Send + 'static) {
    dispatch2::DispatchQueue::main().exec_async(move || autoreleasepool(|_| work()));
}
define_class!(
    #[unsafe(super=NSObject)]
    #[name="StudioNativeMediaItemObserver"]
    #[thread_kind=AnyThread]
    struct ItemObserver;
    unsafe impl NSObjectProtocol for ItemObserver {}
    impl ItemObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn changed(&self,_key:Option<&NSString>,object:Option<&AnyObject>,_change:Option<&NSDictionary<NSKeyValueChangeKey,AnyObject>>,_context:*mut c_void) {
            let token=object.and_then(|object|item_tokens().lock().ok()?.get(&(object as *const AnyObject as usize)).cloned());
            if let Some(token)=token {on_ui(move||item_changed(&token));}
        }
    }
);
// The singleton has no instance state. Its callback only uses the locked token map.
unsafe impl Send for ItemObserver {}
unsafe impl Sync for ItemObserver {}
fn observer() -> &'static Retained<ItemObserver> {
    static OBSERVER: OnceLock<Retained<ItemObserver>> = OnceLock::new();
    OBSERVER.get_or_init(|| unsafe { msg_send![ItemObserver::alloc(), init] })
}
struct WindowIvars {
    token: String,
}
define_class!(
    #[unsafe(super=NSObject)]
    #[name = "StudioNativeMediaWindowDelegate"]
    #[thread_kind=MainThreadOnly]
    #[ivars=WindowIvars]
    struct WindowDelegate;
    unsafe impl NSObjectProtocol for WindowDelegate {}
    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn closing(&self, _notification: &NSNotification) {
            let token = self.ivars().token.clone();
            // Defer past any current player mutation. An old window owns only its token.
            on_ui(move || mac::close_media_token(&token));
        }
    }
);

pub(super) struct Resources {
    asset: Retained<AVURLAsset>,
    item: Option<Retained<AVPlayerItem>>,
    player: Option<Retained<AVPlayer>>,
    observing: bool,
    _directory: tempfile::TempDir,
}
impl Drop for Resources {
    fn drop(&mut self) {
        if let Some(item) = &self.item {
            if let Ok(mut tokens) = item_tokens().lock() {
                tokens.remove(&(Retained::as_ptr(item) as usize));
            }
            if self.observing {
                unsafe {
                    item.removeObserver_forKeyPath(observer(), &NSString::from_str("status"));
                }
            }
        }
        unsafe {
            if let Some(player) = &self.player {
                player.pause();
                player.replaceCurrentItemWithPlayerItem(None);
            }
            self.asset.cancelLoading();
        }
    }
}
struct Pending {
    owner: WeakSender<StoreCommand>,
    request: String,
    name: String,
    resources: Resources,
    reply: oneshot::Sender<Result<PlaybackReceipt, String>>,
}
thread_local! {static PENDING:RefCell<HashMap<String,Pending>>=RefCell::new(HashMap::new());}

pub(super) struct Player {
    window: Retained<NSWindow>,
    _delegate: Retained<WindowDelegate>,
    resources: Resources,
}
impl Player {
    pub(super) fn create(resources: Resources, name: &str, token: &str) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("The native media view requires its UI thread.")?;
        let player = resources
            .player
            .as_ref()
            .ok_or("The native media player is not ready.")?;
        let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(900., 650.));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        let delegate = WindowDelegate::alloc(mtm).set_ivars(WindowIvars {
            token: token.into(),
        });
        // Initialize NSObject after this class's fields have been set.
        let delegate: Retained<WindowDelegate> = unsafe { msg_send![super(delegate), init] };
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // SAFETY: objc2-av-kit 0.3.2 binds this NSView initializer as unsafe.
        // The allocation is fresh and on the main thread. The frame has finite, positive dimensions.
        let view = unsafe { AVPlayerView::initWithFrame(AVPlayerView::alloc(mtm), frame) };
        unsafe {
            view.setPlayer(Some(player));
            view.setAllowsPictureInPicturePlayback(false);
            view.setShowsSharingServiceButton(false);
            view.setUpdatesNowPlayingInfoCenter(false);
            view.setShowsFullScreenToggleButton(true);
            if view.respondsToSelector(sel!(setAllowsVideoFrameAnalysis:)) {
                view.setAllowsVideoFrameAnalysis(false);
            }
        }
        window.setTitle(&NSString::from_str(name));
        window.setContentView(Some(&view));
        window.center();
        window.makeKeyAndOrderFront(None);
        unsafe {
            player.play();
        }
        Ok(Self {
            window,
            _delegate: delegate,
            resources,
        })
    }
    pub(super) fn failed(&self) -> bool {
        self.resources
            .item
            .as_ref()
            .is_some_and(|item| unsafe { item.status() == AVPlayerItemStatus::Failed })
    }
    pub(super) fn stop(self) {
        self.window.setDelegate(None);
        self.window.close();
    }
}

fn key_loaded(asset: &AVURLAsset, key: &str) -> bool {
    // The pinned bindings omit this SDK selector. The SDK declares an NSInteger
    // status and nullable NSError**. No pointer returned here is dereferenced.
    let mut error: *mut NSError = std::ptr::null_mut();
    let status: AVKeyValueStatus = unsafe {
        msg_send![asset, statusOfValueForKey: &*NSString::from_str(key), error: &mut error]
    };
    status == AVKeyValueStatus::Loaded && error.is_null()
}
fn fail_pending(token: &str, error: &str) {
    let pending = PENDING.with_borrow_mut(|items| items.remove(token));
    if let Some(pending) = pending {
        let result = if let Some(owner) = pending.owner.upgrade() {
            let _ = playback::fail(&owner, token, error.into());
            playback::state(&owner, &pending.request)
        } else {
            playback::cancel_native_pending(token);
            Err("The original native media context closed.".into())
        };
        let _ = pending.reply.send(result);
    }
}
fn asset_loaded(token: &str) {
    if !playback::preparing_token(token) {
        cancel_token(token);
        return;
    }
    let asset = PENDING.with_borrow(|items| {
        items
            .get(token)
            .map(|pending| pending.resources.asset.clone())
    });
    let Some(asset) = asset else {
        return;
    };
    if !key_loaded(&asset, "playable")
        || !key_loaded(&asset, "hasProtectedContent")
        || unsafe { !asset.isPlayable() || asset.hasProtectedContent() }
    {
        fail_pending(token, "This media cannot be played from its native file.");
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        fail_pending(token, "The native media UI thread is unavailable.");
        return;
    };
    let item = unsafe { AVPlayerItem::playerItemWithAsset(&asset, mtm) };
    let player = unsafe { AVPlayer::playerWithPlayerItem(Some(&item), mtm) };
    unsafe {
        player.setAllowsExternalPlayback(false);
    }
    if let Ok(mut tokens) = item_tokens().lock() {
        tokens.insert(Retained::as_ptr(&item) as usize, token.into());
    } else {
        fail_pending(token, "The native media observer stopped.");
        return;
    }
    PENDING.with_borrow_mut(|items| {
        if let Some(pending) = items.get_mut(token) {
            pending.resources.item = Some(item.clone());
            pending.resources.player = Some(player);
            pending.resources.observing = true;
        }
    });
    unsafe {
        item.addObserver_forKeyPath_options_context(
            observer(),
            &NSString::from_str("status"),
            NSKeyValueObservingOptions::Initial | NSKeyValueObservingOptions::New,
            std::ptr::null_mut(),
        );
    }
}
fn item_changed(token: &str) {
    let status = PENDING.with_borrow(|items| {
        items
            .get(token)
            .and_then(|pending| pending.resources.item.as_ref())
            .map(|item| unsafe { item.status() })
    });
    match status {
        Some(status) if status == AVPlayerItemStatus::Failed => fail_pending(
            token,
            "This media could not be prepared by the native player.",
        ),
        Some(status) if status == AVPlayerItemStatus::ReadyToPlay => {
            let pending = PENDING.with_borrow_mut(|items| items.remove(token));
            if let Some(pending) = pending {
                let outcome = match pending.owner.upgrade() {
                    Some(owner) => {
                        mac::finish_media(owner, token.to_owned(), pending.resources, pending.name)
                    }
                    None => {
                        playback::cancel_native_pending(token);
                        Err("The original native media context closed.".into())
                    }
                };
                let _ = pending.reply.send(outcome);
            }
        }
        None => mac::fail_media_if_needed(token),
        _ => {}
    }
}
fn begin(
    owner: StoreSender,
    token: String,
    path: String,
    name: String,
    directory: tempfile::TempDir,
    reply: oneshot::Sender<Result<PlaybackReceipt, String>>,
) -> Result<(), String> {
    let receipt = playback::receipt(&owner, &token)?;
    if !playback::preparing(&owner, &token) {
        let _ = reply.send(Ok(receipt));
        return Ok(());
    }
    if reply.is_closed() {
        playback::cancel_native_pending(&token);
        return Ok(());
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
    let restrictions = NSNumber::new_u64(AVAssetReferenceRestrictions::ForbidAll.bits() as u64);
    let options = NSDictionary::from_slices(
        &[unsafe { AVURLAssetReferenceRestrictionsKey }],
        &[restrictions.as_ref() as &AnyObject],
    );
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, Some(&options)) };
    if unsafe { asset.referenceRestrictions() } != AVAssetReferenceRestrictions::ForbidAll {
        return Err("The native player did not restrict external media references.".into());
    }
    let request = receipt.request_id;
    PENDING.with_borrow_mut(|items| {
        items.insert(
            token.clone(),
            Pending {
                owner: owner.downgrade(),
                request,
                name,
                resources: Resources {
                    asset: asset.clone(),
                    item: None,
                    player: None,
                    observing: false,
                    _directory: directory,
                },
                reply,
            },
        )
    });
    let keys = NSArray::from_retained_slice(&[
        NSString::from_str("playable"),
        NSString::from_str("hasProtectedContent"),
    ]);
    let completion = block2::RcBlock::new(move || {
        let token = token.clone();
        on_ui(move || asset_loaded(&token));
    });
    unsafe {
        asset.loadValuesAsynchronouslyForKeys_completionHandler(&keys, Some(&completion));
    }
    Ok(())
}
pub(super) fn cancel_token(token: &str) {
    playback::cancel_native_pending(token);
    let pending = PENDING.with_borrow_mut(|items| items.remove(token));
    if let Some(pending) = pending {
        let value = pending
            .owner
            .upgrade()
            .and_then(|owner| playback::state(&owner, &pending.request).ok())
            .ok_or_else(|| "The original native media context closed.".into());
        let _ = pending.reply.send(value);
    }
}
pub(super) fn cancel_owner(owner: &StoreSender, token: Option<&str>) {
    let ids = PENDING.with_borrow(|items| {
        items
            .iter()
            .filter(|(id, pending)| {
                playback::same_owner(&pending.owner, owner)
                    && token.is_none_or(|token| token == id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    });
    for id in ids {
        cancel_token(&id);
    }
}
pub(super) fn cancel_all() {
    let ids = PENDING.with_borrow(|items| items.keys().cloned().collect::<Vec<_>>());
    for id in ids {
        cancel_token(&id);
    }
}
struct Cancel(String);
impl Drop for Cancel {
    fn drop(&mut self) {
        let token = self.0.clone();
        on_ui(move || cancel_token(&token));
    }
}
pub(super) async fn open(
    owner: StoreSender,
    token: String,
    path: String,
    name: String,
    directory: tempfile::TempDir,
) -> Result<PlaybackReceipt, String> {
    let _cancel = Cancel(token.clone());
    let (reply, receive) = oneshot::channel();
    mac::on_main(move || begin(owner, token, path, name, directory, reply)).await?;
    tokio::time::timeout(MEDIA_READY_TIMEOUT, receive)
        .await
        .map_err(|_| "The native media preparation timed out.")?
        .map_err(|_| "The native media result is not confirmed.")?
}
