//! System Quick Look owns document rendering; no renderer HTML or Electron provider.
use objc2::{MainThreadMarker, MainThreadOnly, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::{NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};
use objc2_quick_look_ui::{QLPreviewItem, QLPreviewView, QLPreviewViewStyle};
use std::cell::RefCell;

thread_local! {
    // One owned preview window. Closing releases its document through QLPreviewView.
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

pub fn close() {
    WINDOW.with_borrow_mut(|slot| {
        if let Some(window) = slot.take() {
            window.close();
        }
    });
}

pub fn show(url: &NSURL, title: &str) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Quick Look requires the main thread.")?;
    close();
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(900.0, 650.0));
    // The generated bindings mark these AppKit initializers unsafe. All objects
    // remain on the main thread and the window owns the preview view.
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
    window.setTitle(&NSString::from_str(title));
    let preview = unsafe {
        QLPreviewView::initWithFrame_style(
            QLPreviewView::alloc(mtm),
            frame,
            QLPreviewViewStyle::Normal,
        )
    }
    .ok_or("Quick Look could not create a preview view.")?;
    unsafe {
        preview.setShouldCloseWithWindow(true);
        preview.setAutostarts(false);
        preview.setPreviewItem(Some(ProtocolObject::<dyn QLPreviewItem>::from_ref(url)));
    }
    window.setContentView(Some(&preview));
    window.center();
    window.makeKeyAndOrderFront(None);
    WINDOW.with_borrow_mut(|slot| *slot = Some(window));
    Ok(())
}
