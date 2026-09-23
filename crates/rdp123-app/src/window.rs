//! A session window: the borderless-ish remote view plus resize and clipboard
//! plumbing. `WindowController` is the window's delegate and the clipboard
//! timer's target.

use std::cell::{Cell, RefCell};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSAlert, NSAlertStyle, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent,
    NSEventModifierFlags, NSFont, NSLineBreakMode, NSModalResponse, NSPasteboard, NSPasteboardItem,
    NSPasteboardTypeFileURL, NSPasteboardTypeString, NSPasteboardWriting, NSProgressIndicator,
    NSProgressIndicatorStyle, NSScreen, NSTextField, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSString, NSTimer, NSURL};
use objc2_quartz_core::kCAFilterLinear;

use rdp123_core::{
    ClipboardMode, RdpOptions, ResolutionMode, ScalingLevel, SessionCommand, SessionHandle,
};

use crate::delegate;
use crate::ui;
use crate::view::RdpView;
use crate::web_auth::WebAuthController;

const CLIPBOARD_POLL_SECONDS: f64 = 0.5;
const CLIPBOARD_STATUS_SECONDS: f64 = 10.0;
const TERMINAL_SHEET_AUTO_CLOSE_SECONDS: f64 = 5.0 * 60.0;
const DEFAULT_CONTENT_W: f64 = 1280.0;
const DEFAULT_CONTENT_H: f64 = 800.0;
const TAB_KEYCODE: u16 = 0x30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKeyDownAction {
    AppKit,
    Forward,
    Pulse,
}

fn session_key_down_action(keycode: u16, modifier_flags: usize) -> SessionKeyDownAction {
    if keycode == TAB_KEYCODE && modifier_flags & NSEventModifierFlags::Control.0 != 0 {
        SessionKeyDownAction::Forward
    } else if modifier_flags & NSEventModifierFlags::Command.0 != 0 {
        SessionKeyDownAction::Pulse
    } else {
        SessionKeyDownAction::AppKit
    }
}

#[derive(Debug, Default)]
struct ClipboardChangeTracker {
    delivered_change_count: Option<isize>,
}

#[derive(Default)]
struct SessionWindowIvars {
    view: RefCell<Option<Retained<RdpView>>>,
}

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123SessionWindow"]
    #[ivars = SessionWindowIvars]
    struct SessionWindow;

    impl SessionWindow {
        #[unsafe(method(sendEvent:))]
        fn send_event(&self, event: &NSEvent) {
            // AppKit reserves Control-Tab for key-view focus and may omit key-up
            // events while Command is held, so handle both cases before super.
            if event.r#type() == objc2_app_kit::NSEventType::KeyDown {
                let keycode = event.keyCode();
                let view = self.ivars().view.borrow().clone();
                if let Some(view) = view {
                    match session_key_down_action(keycode, event.modifierFlags().0) {
                        SessionKeyDownAction::Forward => {
                            view.forward_key_down(keycode, crate::view::key_character(event));
                            return;
                        }
                        SessionKeyDownAction::Pulse => {
                            view.forward_key_pulse(keycode, crate::view::key_character(event));
                            return;
                        }
                        SessionKeyDownAction::AppKit => {}
                    }
                }
            }
            unsafe {
                let _: () = msg_send![super(self), sendEvent: event];
            }
        }
    }
);

impl SessionWindow {
    fn new(
        mtm: MainThreadMarker,
        frame: objc2_foundation::NSRect,
        style: NSWindowStyleMask,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SessionWindowIvars::default());
        unsafe {
            msg_send![
                super(this),
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        }
    }

    fn set_rdp_view(&self, view: &RdpView) {
        *self.ivars().view.borrow_mut() = Some(view.retain());
    }
}

impl ClipboardChangeTracker {
    fn is_pending(&self, change_count: isize) -> bool {
        self.delivered_change_count != Some(change_count)
    }

    fn record_attempt(&mut self, change_count: isize, delivered: bool) {
        if delivered {
            self.delivered_change_count = Some(change_count);
        }
    }

    fn acknowledge_local_write(&mut self, change_count: isize) {
        self.delivered_change_count = Some(change_count);
    }
}

#[derive(Default)]
pub struct WindowControllerIvars {
    window: RefCell<Option<Retained<SessionWindow>>>,
    view: RefCell<Option<Retained<RdpView>>>,
    handle: RefCell<Option<SessionHandle>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    status_overlay: RefCell<Option<Retained<NSView>>>,
    status_label: RefCell<Option<Retained<NSTextField>>>,
    status_spinner: RefCell<Option<Retained<NSProgressIndicator>>>,
    terminal_alert: RefCell<Option<Retained<NSAlert>>>,
    terminal_timer: RefCell<Option<Retained<NSTimer>>>,
    clipboard_status_timer: RefCell<Option<Retained<NSTimer>>>,
    web_auth: RefCell<Option<Retained<WebAuthController>>>,
    connection_id: RefCell<String>,
    title: RefCell<String>,
    window_id: Cell<u64>,
    scaling: Cell<ScalingLevel>,
    resolution_mode: Cell<ResolutionMode>,
    fixed_resolution: Cell<Option<(u16, u16)>>,
    remember_size: Cell<bool>,
    clipboard_mode: Cell<ClipboardMode>,
    clipboard_tracker: RefCell<ClipboardChangeTracker>,
    /// Pasteboard generation reserved while remote files are materialized.
    /// A newer local copy wins and prevents a completed background transfer
    /// from overwriting the user's newer clipboard contents.
    remote_file_offer_change_count: Cell<Option<isize>>,
}

define_class!(
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123WindowController"]
    #[ivars = WindowControllerIvars]
    pub struct WindowController;

    unsafe impl NSObjectProtocol for WindowController {}

    unsafe impl NSWindowDelegate for WindowController {
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            // Live-drag steps are ignored; the end-of-resize handler covers them.
            // This still fires for full-screen and programmatic changes.
            let in_live = self
                .ivars()
                .view
                .borrow()
                .as_ref()
                .map(|v| v.inLiveResize())
                .unwrap_or(false);
            if !in_live {
                self.send_resize();
            }
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            self.send_resize();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            if let Some(view) = self.ivars().view.borrow().as_ref() {
                view.release_all_keys();
            }
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            // Synchronize before the user can type ⌘V. The periodic timer is
            // only a fallback for changes made while this window stays active.
            self.poll_clipboard_impl();
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if let Some(timer) = self.ivars().timer.borrow_mut().take() {
                timer.invalidate();
            }
            if let Some(timer) = self.ivars().terminal_timer.borrow_mut().take() {
                timer.invalidate();
            }
            if let Some(timer) = self.ivars().clipboard_status_timer.borrow_mut().take() {
                timer.invalidate();
            }
            if let Some(view) = self.ivars().view.borrow().as_ref() {
                view.release_all_keys();
            }
            if let Some(handle) = self.ivars().handle.borrow().as_ref() {
                handle.command(SessionCommand::Shutdown);
            }
            if let Some(web_auth) = self.ivars().web_auth.borrow_mut().take() {
                web_auth.cancel();
            }
            self.save_size_if_enabled();
            // Deferred: dropping the last controller retain inside its own
            // delegate callback would deallocate `self` mid-call.
            let window_id = self.ivars().window_id.get();
            DispatchQueue::main().exec_async(move || delegate::remove_window(window_id));
        }
    }

    impl WindowController {
        #[unsafe(method(pollClipboard:))]
        fn poll_clipboard(&self, _timer: &NSTimer) {
            self.poll_clipboard_impl();
        }

        #[unsafe(method(dismissTerminalSheet:))]
        fn dismiss_terminal_sheet(&self, _timer: &NSTimer) {
            let Some(alert) = self.ivars().terminal_alert.borrow().clone() else {
                return;
            };
            let Some(window) = self.ivars().window.borrow().clone() else {
                return;
            };
            window.endSheet(&alert.window());
        }

        #[unsafe(method(restoreClipboardTitle:))]
        fn restore_clipboard_title(&self, _timer: &NSTimer) {
            self.ivars().clipboard_status_timer.borrow_mut().take();
            self.restore_connection_title();
        }
    }
);

impl WindowController {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowControllerIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    /// Build and show the window. Call `initial_size` afterwards to learn the
    /// framebuffer size to request, then `attach_session`.
    pub fn setup(
        &self,
        mtm: MainThreadMarker,
        window_id: u64,
        connection_id: &str,
        title: &str,
        opts: &RdpOptions,
        external_stt_paste: bool,
    ) {
        self.ivars().window_id.set(window_id);
        self.ivars().scaling.set(opts.scaling);
        self.ivars().resolution_mode.set(opts.resolution_mode);
        self.ivars().fixed_resolution.set(opts.resolution);
        self.ivars().remember_size.set(opts.remember_size);
        self.ivars().clipboard_mode.set(opts.clipboard);
        *self.ivars().connection_id.borrow_mut() = connection_id.to_string();
        *self.ivars().title.borrow_mut() = title.to_string();

        // Initial window size: fixed resolution (clamped), the remembered last
        // size (fit-to-window), or a default. A full-screen start sizes to the
        // screen up front, because the session connects before the async
        // full-screen transition finishes.
        let (content_width, content_height) = match (opts.resolution_mode, opts.resolution) {
            (ResolutionMode::Fixed, Some((w, h))) => {
                (f64::from(w).min(2400.0), f64::from(h).min(1400.0))
            }
            _ if opts.fullscreen => match NSScreen::mainScreen(mtm) {
                Some(screen) => {
                    let size = screen.visibleFrame().size;
                    (size.width, size.height)
                }
                None => (DEFAULT_CONTENT_W, DEFAULT_CONTENT_H),
            },
            _ => match opts.last_window_size.filter(|_| opts.remember_size) {
                Some((w, h)) => (
                    f64::from(w).clamp(480.0, 4000.0),
                    f64::from(h).clamp(360.0, 3000.0),
                ),
                None => (DEFAULT_CONTENT_W, DEFAULT_CONTENT_H),
            },
        };

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let frame = ui::rect(content_width, content_height);
        let window = SessionWindow::new(mtm, frame, style);
        window.setTitle(&NSString::from_str(&format!("{title} — connecting…")));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setAcceptsMouseMovedEvents(true);

        let view = RdpView::new(mtm, ui::rect(content_width, content_height));
        window.set_rdp_view(&view);
        view.set_external_stt_paste_enabled(
            external_stt_paste && opts.clipboard.allow_local_to_remote(),
        );
        view.setWantsLayer(true);
        if let Some(layer) = view.layer() {
            layer.setContentsScale(window.backingScaleFactor());
            // Linear (not nearest) so a scaled framebuffer isn't blocky/pixelated.
            unsafe { layer.setMagnificationFilter(kCAFilterLinear) };
        }

        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                view.bounds(),
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);

        let status_overlay =
            NSView::initWithFrame(NSView::alloc(mtm), ui::rect(content_width, content_height));
        status_overlay.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        status_overlay.setWantsLayer(true);
        if let Some(layer) = status_overlay.layer() {
            let shade = NSColor::colorWithWhite_alpha(0.04, 0.72);
            layer.setBackgroundColor(Some(&shade.CGColor()));
        }
        view.addSubview(&status_overlay);

        let spinner = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            ui::rect(32.0, 32.0),
        );
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setIndeterminate(true);
        spinner.setDisplayedWhenStopped(false);
        spinner.setFrameOrigin(objc2_foundation::NSPoint::new(
            (content_width - 32.0) / 2.0,
            content_height / 2.0 + 8.0,
        ));
        spinner.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        unsafe { spinner.startAnimation(None) };
        status_overlay.addSubview(&spinner);

        let status_label = NSTextField::labelWithString(&NSString::from_str("Connecting…"), mtm);
        status_label.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        status_label.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
        status_label.setTextColor(Some(&NSColor::whiteColor()));
        status_label.setMaximumNumberOfLines(4);
        status_label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        status_label.setFrame(objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(32.0, content_height / 2.0 - 88.0),
            objc2_core_foundation::CGSize::new(content_width - 64.0, 120.0),
        ));
        status_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        status_overlay.addSubview(&status_label);

        window.setContentView(Some(&view));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        if opts.fullscreen {
            window.toggleFullScreen(None);
        }

        *self.ivars().window.borrow_mut() = Some(window);
        *self.ivars().view.borrow_mut() = Some(view);
        *self.ivars().status_overlay.borrow_mut() = Some(status_overlay);
        *self.ivars().status_label.borrow_mut() = Some(status_label);
        *self.ivars().status_spinner.borrow_mut() = Some(spinner);
    }

    /// Whether the remote resolution should follow the window (fit-to-window).
    pub fn dynamic_resolution(&self) -> bool {
        self.ivars().resolution_mode.get() == ResolutionMode::FitToWindow
    }

    /// The framebuffer size (and optional scale) to request for this window.
    pub fn initial_size(&self) -> (u16, u16, Option<u32>) {
        self.current_size()
    }

    pub fn attach_session(&self, handle: SessionHandle) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_session(handle.clone());
        }
        *self.ivars().handle.borrow_mut() = Some(handle);
        if self.ivars().clipboard_mode.get().allow_local_to_remote() {
            self.start_clipboard_timer();
        }
    }

    pub fn set_external_stt_paste_enabled(&self, enabled: bool) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_external_stt_paste_enabled(
                enabled && self.ivars().clipboard_mode.get().allow_local_to_remote(),
            );
        }
    }

    pub fn set_mac_shortcuts_enabled(&self, enabled: bool) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_mac_shortcuts_enabled(enabled);
        }
    }

    pub fn begin_entra_sign_in(
        &self,
        mtm: MainThreadMarker,
        authorization_url: &str,
        redirect_uri: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    ) {
        if let Some(previous) = self.ivars().web_auth.borrow_mut().take() {
            previous.cancel();
        }
        let web_auth = WebAuthController::new(mtm, redirect_uri, reply);
        if let Err(error) = web_auth.show(mtm, authorization_url) {
            web_auth.cancel();
            ui::show_error(mtm, "Could not open Microsoft sign-in", &error);
            return;
        }
        *self.ivars().web_auth.borrow_mut() = Some(web_auth);
    }

    pub fn end_entra_sign_in(&self) {
        self.ivars().web_auth.borrow_mut().take();
    }

    /// Raise the session after the app's activation policy has switched from
    /// menu-bar accessory to a regular foreground application.
    pub fn bring_to_front(&self) {
        let window = self.ivars().window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        if window.isMiniaturized() {
            window.deminiaturize(None);
        }
        window.makeKeyAndOrderFront(None);
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            window.makeFirstResponder(Some(view));
        }
    }

    /// User-facing connection name shown in the Dock menu.
    pub fn title(&self) -> String {
        self.ivars().title.borrow().clone()
    }

    /// Whether this session is currently frontmost, represented by a checkmark
    /// in the Dock menu.
    pub fn is_key_window(&self) -> bool {
        self.ivars()
            .window
            .borrow()
            .as_ref()
            .is_some_and(|window| window.isKeyWindow())
    }

    pub fn connection_id(&self) -> String {
        self.ivars().connection_id.borrow().clone()
    }

    /// Persist the current window size so the next connect reopens at this size.
    /// Only for fit-to-window sessions that aren't full-screen.
    fn save_size_if_enabled(&self) {
        if !self.ivars().remember_size.get()
            || self.ivars().resolution_mode.get() != ResolutionMode::FitToWindow
        {
            return;
        }
        let window = self.ivars().window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
            return;
        }
        let view = self.ivars().view.borrow();
        let Some(view) = view.as_ref() else { return };
        let size = view.bounds().size;
        let (w, h) = (size.width as u16, size.height as u16);
        if w >= 480 && h >= 360 {
            delegate::save_window_size(&self.ivars().connection_id.borrow(), w, h);
        }
    }

    pub fn refresh(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.refresh();
        }
    }

    pub fn set_pointer_bitmap(
        &self,
        rgba: Vec<u8>,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
    ) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_bitmap(rgba, width, height, hotspot_x, hotspot_y);
        }
    }

    pub fn set_pointer_default(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_default();
        }
    }

    pub fn set_pointer_hidden(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_hidden();
        }
    }

    fn cancel_clipboard_status_timer(&self) {
        if let Some(timer) = self.ivars().clipboard_status_timer.borrow_mut().take() {
            timer.invalidate();
        }
    }

    fn restore_connection_title(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.setTitle(&NSString::from_str(&self.ivars().title.borrow()));
        }
    }

    fn set_window_title_status(&self, status: &str) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            let base = self.ivars().title.borrow();
            window.setTitle(&NSString::from_str(&format!("{base} — {status}")));
        }
    }

    fn set_temporary_clipboard_status(&self, status: &str) {
        self.cancel_clipboard_status_timer();
        self.set_window_title_status(status);
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                CLIPBOARD_STATUS_SECONDS,
                self as &AnyObject,
                sel!(restoreClipboardTitle:),
                None,
                false,
            )
        };
        *self.ivars().clipboard_status_timer.borrow_mut() = Some(timer);
    }

    fn show_clipboard_file_failure(&self) {
        self.set_temporary_clipboard_status("file copy failed");
    }

    pub fn set_connected(&self) {
        self.cancel_clipboard_status_timer();
        self.restore_connection_title();
        if let Some(overlay) = self.ivars().status_overlay.borrow().as_ref() {
            overlay.setHidden(true);
        }
        if let Some(spinner) = self.ivars().status_spinner.borrow().as_ref() {
            unsafe { spinner.stopAnimation(None) };
        }
    }

    /// Dim the last remote frame so it is visibly stale while reconnecting.
    pub fn set_reconnecting(&self, attempt: u32, max_attempts: u32) {
        self.cancel_clipboard_status_timer();
        self.ivars().remote_file_offer_change_count.set(None);
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            let base = self.ivars().title.borrow().clone();
            window.setTitle(&NSString::from_str(&format!("{base} — reconnecting…")));
        }
        if let Some(overlay) = self.ivars().status_overlay.borrow().as_ref() {
            overlay.setHidden(false);
        }
        if let Some(label) = self.ivars().status_label.borrow().as_ref() {
            label.setStringValue(&NSString::from_str(&format!(
                "Connection interrupted\nReconnecting automatically…\nAttempt {attempt} of {max_attempts}"
            )));
        }
        if let Some(spinner) = self.ivars().status_spinner.borrow().as_ref() {
            spinner.setHidden(false);
            unsafe { spinner.stopAnimation(None) };
            unsafe { spinner.startAnimation(None) };
        }
    }

    /// Present a document-modal failure sheet. Unlike `NSAlert::runModal`, this
    /// blocks only the ended session window; the OK button and automatic close
    /// both finish the sheet through AppKit's supported modal-session path.
    pub fn show_terminal_sheet(&self, mtm: MainThreadMarker, heading: &str, message: &str) {
        if self.ivars().terminal_alert.borrow().is_some() {
            return;
        }
        if let Some(timer) = self.ivars().timer.borrow_mut().take() {
            timer.invalidate();
        }
        self.cancel_clipboard_status_timer();
        self.ivars().remote_file_offer_change_count.set(None);
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.release_all_keys();
        }
        self.ivars().handle.borrow_mut().take();

        let Some(window) = self.ivars().window.borrow().clone() else {
            return;
        };
        let base = self.ivars().title.borrow().clone();
        window.setTitle(&NSString::from_str(&format!(
            "{base} — {}",
            heading.to_lowercase()
        )));
        if let Some(overlay) = self.ivars().status_overlay.borrow().as_ref() {
            overlay.setHidden(false);
        }
        if let Some(label) = self.ivars().status_label.borrow().as_ref() {
            label.setStringValue(&NSString::from_str(&format!("{heading}\n{message}")));
        }
        if let Some(spinner) = self.ivars().status_spinner.borrow().as_ref() {
            unsafe { spinner.stopAnimation(None) };
            spinner.setHidden(true);
        }

        let alert = NSAlert::new(mtm);
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&NSString::from_str(heading));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        *self.ivars().terminal_alert.borrow_mut() = Some(alert.clone());

        let controller = self.retain();
        let completion = RcBlock::new(move |_response: NSModalResponse| {
            if let Some(timer) = controller.ivars().terminal_timer.borrow_mut().take() {
                timer.invalidate();
            }
            controller.ivars().terminal_alert.borrow_mut().take();
            controller.close();
        });
        alert.beginSheetModalForWindow_completionHandler(&window, Some(&completion));

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                TERMINAL_SHEET_AUTO_CLOSE_SECONDS,
                self as &AnyObject,
                sel!(dismissTerminalSheet:),
                None,
                false,
            )
        };
        *self.ivars().terminal_timer.borrow_mut() = Some(timer);
    }

    pub fn close(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
    }

    /// Write remote clipboard text locally without triggering our own poll.
    pub fn set_clipboard(&self, text: &str) {
        if !self.ivars().clipboard_mode.get().allow_remote_to_local() {
            return;
        }
        self.ivars().remote_file_offer_change_count.set(None);
        self.cancel_clipboard_status_timer();
        self.restore_connection_title();
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let ok = unsafe {
            pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString)
        };
        let _ = ok;
        self.ivars()
            .clipboard_tracker
            .borrow_mut()
            .acknowledge_local_write(pasteboard.changeCount());
    }

    /// The remote framebuffer size (and optional server scale %) to request.
    fn current_size(&self) -> (u16, u16, Option<u32>) {
        let scaling = self.ivars().scaling.get();
        // Fixed resolution: request exactly the chosen size; the view scales it.
        if self.ivars().resolution_mode.get() == ResolutionMode::Fixed {
            let (w, h) = self
                .ivars()
                .fixed_resolution
                .get()
                .unwrap_or((DEFAULT_CONTENT_W as u16, DEFAULT_CONTENT_H as u16));
            return (w & !1, h, scaling.percent().or(Some(100)));
        }

        // Fit to window: size follows the view.
        let view = self.ivars().view.borrow();
        let Some(view) = view.as_ref() else {
            return (0, 0, None);
        };
        let bounds = view.bounds();
        let retina = self
            .ivars()
            .window
            .borrow()
            .as_ref()
            .map(|w| w.backingScaleFactor() >= 1.5)
            .unwrap_or(false);

        match scaling.percent() {
            // Explicit DPI: request physical pixels at that scale.
            Some(pct) => {
                let backing = view.convertSizeToBacking(bounds.size);
                (even(backing.width), backing.height as u16, Some(pct))
            }
            // Auto: Retina => physical pixels at 200%, otherwise points at 100%.
            None if retina => {
                let backing = view.convertSizeToBacking(bounds.size);
                (even(backing.width), backing.height as u16, Some(200))
            }
            None => (
                even(bounds.size.width),
                bounds.size.height as u16,
                Some(100),
            ),
        }
    }

    fn send_resize(&self) {
        let (width, height, scale) = self.current_size();
        if width == 0 || height == 0 {
            return;
        }
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::Resize {
                width,
                height,
                scale,
            });
        }
    }

    /// Reserve the macOS clipboard while the session materializes the remote
    /// selection. Clearing now prevents Finder from pasting stale local data.
    pub fn prepare_remote_files(&self, count: usize) {
        if !self.ivars().clipboard_mode.get().allow_remote_to_local() {
            return;
        }
        if count == 0 {
            return;
        }
        self.cancel_clipboard_status_timer();
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let change_count = pasteboard.changeCount();
        self.ivars()
            .clipboard_tracker
            .borrow_mut()
            .acknowledge_local_write(change_count);
        self.ivars()
            .remote_file_offer_change_count
            .set(Some(change_count));
        let noun = if count == 1 { "file" } else { "files" };
        self.set_window_title_status(&format!("preparing {count} {noun}…"));
    }

    /// Publish fully downloaded files as real `public.file-url` pasteboard
    /// items. Finder can copy these with its normal Command-V path.
    pub fn offer_remote_files(&self, paths: Vec<std::path::PathBuf>) {
        if !self.ivars().clipboard_mode.get().allow_remote_to_local() {
            return;
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        let expected_change = self.ivars().remote_file_offer_change_count.take();
        if expected_change != Some(pasteboard.changeCount()) {
            tracing::debug!(
                "clipboard: not publishing remote files because the local clipboard changed"
            );
            remove_remote_file_cache(&paths);
            self.cancel_clipboard_status_timer();
            self.restore_connection_title();
            return;
        }
        let items = file_url_pasteboard_items(&paths);
        if items.len() != paths.len() || items.is_empty() {
            tracing::warn!("clipboard: downloaded remote files could not be represented locally");
            remove_remote_file_cache(&paths);
            self.show_clipboard_file_failure();
            return;
        }
        let writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = items
            .into_iter()
            .map(|item| {
                ProtocolObject::from_retained(item)
                    as Retained<ProtocolObject<dyn NSPasteboardWriting>>
            })
            .collect();
        pasteboard.clearContents();
        let array = objc2_foundation::NSArray::from_retained_slice(&writers);
        let ok = pasteboard.writeObjects(&array);
        if !ok {
            tracing::warn!("clipboard: could not publish downloaded files to the pasteboard");
            remove_remote_file_cache(&paths);
            self.show_clipboard_file_failure();
            return;
        }
        // Don't re-read our own pasteboard write on the next poll.
        self.ivars()
            .clipboard_tracker
            .borrow_mut()
            .acknowledge_local_write(pasteboard.changeCount());
        let noun = if paths.len() == 1 { "file" } else { "files" };
        self.set_temporary_clipboard_status(&format!("{} {noun} ready to paste", paths.len()));
    }

    pub fn remote_file_preparation_failed(&self, message: &str) {
        tracing::warn!("clipboard: preparing remote files failed: {message}");
        self.ivars().remote_file_offer_change_count.set(None);
        self.show_clipboard_file_failure();
    }

    fn start_clipboard_timer(&self) {
        // Seed the session with clipboard content that already existed before
        // the RDP window opened.
        self.poll_clipboard_impl();
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                CLIPBOARD_POLL_SECONDS,
                self as &AnyObject,
                sel!(pollClipboard:),
                None,
                true,
            )
        };
        *self.ivars().timer.borrow_mut() = Some(timer);
    }

    fn poll_clipboard_impl(&self) {
        if !self.ivars().clipboard_mode.get().allow_local_to_remote() {
            return;
        }
        let is_key = self
            .ivars()
            .window
            .borrow()
            .as_ref()
            .map(|w| w.isKeyWindow())
            .unwrap_or(false);
        if !is_key {
            return;
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        let count = pasteboard.changeCount();
        if !self.ivars().clipboard_tracker.borrow().is_pending(count) {
            return;
        }

        // Files take precedence over text: a Finder copy also puts the file
        // names on the pasteboard as a string.
        let files = pasteboard_file_paths(&pasteboard);
        if !files.is_empty() {
            let delivered = self.ivars().handle.borrow().as_ref().is_some_and(|handle| {
                handle.try_command(SessionCommand::LocalClipboardFiles(files))
            });
            self.ivars()
                .clipboard_tracker
                .borrow_mut()
                .record_attempt(count, delivered);
            return;
        }
        if let Some(text) = unsafe { pasteboard.stringForType(NSPasteboardTypeString) } {
            let text = text.to_string();
            if !text.is_empty() {
                let delivered =
                    self.ivars().handle.borrow().as_ref().is_some_and(|handle| {
                        handle.try_command(SessionCommand::LocalClipboard(text))
                    });
                self.ivars()
                    .clipboard_tracker
                    .borrow_mut()
                    .record_attempt(count, delivered);
                return;
            }
        }
        // Unsupported or empty clipboard contents cannot be redirected; avoid
        // polling the same change forever.
        self.ivars()
            .clipboard_tracker
            .borrow_mut()
            .record_attempt(count, true);
    }
}

#[cfg(test)]
mod keyboard_tests {
    use super::{session_key_down_action, SessionKeyDownAction, TAB_KEYCODE};
    use objc2_app_kit::NSEventModifierFlags;

    #[test]
    fn control_tab_is_forwarded_before_appkit_changes_focus() {
        assert_eq!(
            session_key_down_action(TAB_KEYCODE, NSEventModifierFlags::Control.0),
            SessionKeyDownAction::Forward
        );
        assert_eq!(
            session_key_down_action(
                TAB_KEYCODE,
                NSEventModifierFlags::Control.0 | NSEventModifierFlags::Shift.0,
            ),
            SessionKeyDownAction::Forward
        );
    }

    #[test]
    fn command_modified_keys_are_pulsed_before_appkit_drops_key_up() {
        assert_eq!(
            session_key_down_action(0x0f, NSEventModifierFlags::Command.0),
            SessionKeyDownAction::Pulse
        );
        assert_eq!(
            session_key_down_action(0x30, NSEventModifierFlags::Option.0),
            SessionKeyDownAction::AppKit
        );
    }
}

fn even(v: f64) -> u16 {
    let n = v as u16;
    n & !1
}

/// One modern file-URL pasteboard item per downloaded top-level path. Finder
/// treats these like files copied locally and performs its normal copy on
/// Command-V.
fn file_url_pasteboard_items(paths: &[std::path::PathBuf]) -> Vec<Retained<NSPasteboardItem>> {
    paths
        .iter()
        .filter_map(|path| {
            if !path.exists() {
                return None;
            }
            let path_string = NSString::from_str(&path.to_string_lossy());
            let url = NSURL::fileURLWithPath_isDirectory(&path_string, path.is_dir());
            let absolute = url.absoluteString()?;
            let item = NSPasteboardItem::new();
            unsafe {
                item.setString_forType(&absolute, NSPasteboardTypeFileURL)
                    .then_some(item)
            }
        })
        .collect()
}

fn remove_remote_file_cache(paths: &[std::path::PathBuf]) {
    let expected_root = std::env::temp_dir().join("RDP123").join("Clipboard");
    let Some(cache_dir) = paths.first().and_then(|path| path.parent()) else {
        return;
    };
    if cache_dir.parent() == Some(expected_root.as_path()) {
        let _ = std::fs::remove_dir_all(cache_dir);
    }
}

/// File URLs currently on the pasteboard, as local paths.
fn pasteboard_file_paths(pasteboard: &NSPasteboard) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    let Some(items) = pasteboard.pasteboardItems() else {
        return paths;
    };
    let file_url_type = NSString::from_str("public.file-url");
    for item in items {
        let Some(url_string) = item.stringForType(&file_url_type) else {
            continue;
        };
        let Some(url) = NSURL::URLWithString(&url_string) else {
            continue;
        };
        if let Some(path) = url.path() {
            paths.push(std::path::PathBuf::from(path.to_string()));
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::{file_url_pasteboard_items, ClipboardChangeTracker};
    use objc2_app_kit::NSPasteboardTypeFileURL;

    #[test]
    fn existing_clipboard_content_is_pending_for_a_new_session() {
        let tracker = ClipboardChangeTracker::default();
        assert!(tracker.is_pending(42));
    }

    #[test]
    fn failed_clipboard_enqueue_remains_pending_until_delivery() {
        let mut tracker = ClipboardChangeTracker::default();

        tracker.record_attempt(42, false);
        assert!(tracker.is_pending(42));

        tracker.record_attempt(42, true);
        assert!(!tracker.is_pending(42));
    }

    #[test]
    fn downloaded_files_become_individual_file_url_items() {
        let root =
            std::env::temp_dir().join(format!("rdp123-file-url-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder")).unwrap();
        std::fs::write(root.join("report.txt"), b"ready").unwrap();

        let paths = vec![root.join("report.txt"), root.join("folder")];
        let items = file_url_pasteboard_items(&paths);

        assert_eq!(items.len(), 2);
        for (item, path) in items.iter().zip(&paths) {
            let value = unsafe { item.stringForType(NSPasteboardTypeFileURL) }
                .expect("file URL pasteboard type");
            let url = objc2_foundation::NSURL::URLWithString(&value).expect("valid file URL");
            assert_eq!(
                url.path().expect("local path").to_string(),
                path.to_string_lossy()
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }
}
