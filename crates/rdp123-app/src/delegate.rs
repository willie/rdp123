//! The application delegate: menu-bar item, connection menu, connect flow, and
//! the bridge that delivers session events back onto the main thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSControlStateValueOn,
    NSEventModifierFlags, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{ns_string, NSNotification, NSObject, NSObjectProtocol, NSString};

use rdp123_core::{
    secrets, spawn_session, terminal, AuthenticationMode, Connection, ConnectionKind,
    PasswordPolicy, ProfileStore, SessionConfig, SessionEvent,
};

use crate::settings::SettingsController;
use crate::ui;
use crate::window::WindowController;

#[derive(Debug, PartialEq, Eq)]
enum TerminalSessionPresentation<'a> {
    CloseSilently,
    SheetAndClose {
        title: &'static str,
        message: &'a str,
    },
}

#[derive(Clone, Copy)]
enum TerminalSessionKind {
    Disconnected,
    ConnectionError,
    ReconnectFailed,
}

fn terminal_session_presentation(
    kind: TerminalSessionKind,
    reason: &str,
) -> TerminalSessionPresentation<'_> {
    if matches!(kind, TerminalSessionKind::Disconnected) && reason == rdp123_core::REMOTE_ENDED {
        TerminalSessionPresentation::CloseSilently
    } else {
        TerminalSessionPresentation::SheetAndClose {
            title: match kind {
                TerminalSessionKind::Disconnected => "Disconnected",
                TerminalSessionKind::ConnectionError => "Connection failed",
                TerminalSessionKind::ReconnectFailed => "Reconnect failed",
            },
            message: reason,
        }
    }
}

thread_local! {
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

/// Deliver a session event to its window. Called on the main thread.
pub fn deliver(window_id: u64, event: SessionEvent) {
    let delegate = DELEGATE.with(|d| d.borrow().clone());
    if let Some(delegate) = delegate {
        delegate.handle_session_event(window_id, event);
    }
}

/// Forget a closed window. Called from `windowWillClose:` on the main thread.
pub fn remove_window(window_id: u64) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            delegate.ivars().borrow_mut().windows.remove(&window_id);
            delegate.update_activation_policy();
        }
    });
}

/// Persist a session window's size for its connection. Called on close.
pub fn save_window_size(connection_id: &str, width: u16, height: u16) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            let store = delegate.ivars().borrow().store.clone();
            if let Err(e) = store.set_last_window_size(connection_id, (width, height)) {
                tracing::warn!("could not save window size: {e}");
            }
        }
    });
}

/// Apply the global external-STT paste preference to every open RDP window.
pub fn set_external_stt_paste_enabled(enabled: bool) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            for controller in delegate.ivars().borrow().windows.values() {
                controller.set_external_stt_paste_enabled(enabled);
            }
        }
    });
}

/// Apply the global Mac shortcuts preference to every open RDP window.
pub fn set_mac_shortcuts_enabled(enabled: bool) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            for controller in delegate.ivars().borrow().windows.values() {
                controller.set_mac_shortcuts_enabled(enabled);
            }
        }
    });
}

pub struct AppState {
    store: ProfileStore,
    /// Snapshot backing the current menu; menu tags index into this.
    connections: Vec<Connection>,
    status_item: Option<Retained<NSStatusItem>>,
    settings: Option<Retained<SettingsController>>,
    windows: HashMap<u64, Retained<WindowController>>,
    next_id: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123AppDelegate"]
    #[ivars = RefCell<AppState>]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            DELEGATE.with(|d| *d.borrow_mut() = Some(self.retain()));

            // Key equivalents (⌘V, ⌘C, …) are dispatched through the main
            // menu; without one, text fields cannot paste. The menu is never
            // visible for a menu-bar app, but it must exist.
            install_main_menu(mtm);

            let menu = NSMenu::new(mtm);
            menu.setAutoenablesItems(false);
            let delegate: &ProtocolObject<dyn NSMenuDelegate> = ProtocolObject::from_ref(self);
            menu.setDelegate(Some(delegate));

            let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = status.button(mtm) {
                match ui::menu_bar_icon() {
                    Some(icon) => button.setImage(Some(&icon)),
                    None => button.setTitle(&NSString::from_str("RDP")),
                }
            }
            status.setMenu(Some(&menu));
            self.rebuild_menu(&menu, mtm);
            self.ivars().borrow_mut().status_item = Some(status);

            // `open RDP123.app --args --settings` opens Settings at launch.
            if std::env::args().any(|arg| arg == "--settings") {
                self.show_settings();
            }
        }

        /// Add every live RDP session to the Dock icon's context menu. macOS
        /// gives one Dock icon to the application process, so this is the
        /// native way to make its individual session windows directly
        /// addressable.
        #[unsafe(method_id(applicationDockMenu:))]
        fn application_dock_menu(&self, _sender: &NSApplication) -> Option<Retained<NSMenu>> {
            Some(self.build_dock_menu())
        }
    }

    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let mtm = self.mtm();
            self.rebuild_menu(menu, mtm);
        }
    }

    impl AppDelegate {
        #[unsafe(method(openConnection:))]
        fn open_connection(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let tag: isize = unsafe { msg_send![sender, tag] };
            if tag >= 0 {
                self.open_index(tag as usize);
            }
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.show_settings();
        }

        #[unsafe(method(showSessionFromDock:))]
        fn show_session_from_dock(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Ok(window_id) = u64::try_from(tag) else {
                return;
            };
            self.show_session(window_id);
        }

        /// Quit via our own selector: macOS attaches an automatic icon to
        /// menu items wired to the well-known `terminate:` action, which
        /// breaks the menu's left alignment.
        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }

    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let store = ProfileStore::open_default().expect("resolve config directory");
        let state = AppState {
            store,
            connections: Vec::new(),
            status_item: None,
            settings: None,
            windows: HashMap::new(),
            next_id: 1,
        };
        let this = Self::alloc(mtm).set_ivars(RefCell::new(state));
        unsafe { msg_send![super(this), init] }
    }

    fn rebuild_menu(&self, menu: &NSMenu, mtm: MainThreadMarker) {
        menu.removeAllItems();

        let store = self.ivars().borrow().store.clone();
        let (connections, load_failed) = match store.load() {
            Ok(connections) => (connections, false),
            Err(error) => {
                tracing::error!("could not load connections: {error:#}");
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str("Connections file is invalid — open Settings"),
                        None,
                        ns_string!(""),
                    )
                };
                item.setEnabled(false);
                menu.addItem(&item);
                (Vec::new(), true)
            }
        };

        if connections.is_empty() && !load_failed {
            let empty = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("No connections — edit to add"),
                    None,
                    ns_string!(""),
                )
            };
            empty.setEnabled(false);
            menu.addItem(&empty);
        }

        for (index, connection) in connections.iter().enumerate() {
            let label = match connection.kind {
                ConnectionKind::Rdp => connection.name.clone(),
                ConnectionKind::Ssh => format!("{} (SSH)", connection.name),
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&label),
                    Some(sel!(openConnection:)),
                    ns_string!(""),
                )
            };
            item.setTag(index as isize);
            let target: &AnyObject = self;
            unsafe { item.setTarget(Some(target)) };
            menu.addItem(&item);
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let settings = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Settings…"),
                Some(sel!(openSettings:)),
                ns_string!(","),
            )
        };
        let target: &AnyObject = self;
        unsafe { settings.setTarget(Some(target)) };
        menu.addItem(&settings);

        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit RDP123"),
                Some(sel!(quitApp:)),
                ns_string!("q"),
            )
        };
        let target: &AnyObject = self;
        unsafe { quit.setTarget(Some(target)) };
        menu.addItem(&quit);

        self.ivars().borrow_mut().connections = connections;
    }

    fn open_index(&self, index: usize) {
        let connection = self.ivars().borrow().connections.get(index).cloned();
        let Some(connection) = connection else { return };
        match connection.kind {
            ConnectionKind::Rdp => self.open_rdp(connection),
            ConnectionKind::Ssh => self.open_ssh(&connection),
        }
    }

    fn build_dock_menu(&self) -> Retained<NSMenu> {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);

        let mut sessions: Vec<_> = self
            .ivars()
            .borrow()
            .windows
            .iter()
            .filter_map(|(&window_id, controller)| {
                let tag = isize::try_from(window_id).ok()?;
                Some((tag, controller.title(), controller.is_key_window()))
            })
            .collect();
        sessions.sort_by(|a, b| {
            a.1.to_lowercase()
                .cmp(&b.1.to_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });

        for (tag, title, is_key_window) in sessions {
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&title),
                    Some(sel!(showSessionFromDock:)),
                    ns_string!(""),
                )
            };
            item.setTag(tag);
            if is_key_window {
                item.setState(NSControlStateValueOn);
            }
            let target: &AnyObject = self;
            unsafe { item.setTarget(Some(target)) };
            menu.addItem(&item);
        }

        menu
    }

    fn show_session(&self, window_id: u64) {
        let controller = self.ivars().borrow().windows.get(&window_id).cloned();
        let Some(controller) = controller else { return };

        let app = NSApplication::sharedApplication(self.mtm());
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        controller.bring_to_front();
    }

    fn open_ssh(&self, connection: &Connection) {
        let store = self.ivars().borrow().store.clone();
        let settings = match store.load_document() {
            Ok(document) => document.settings,
            Err(error) => {
                ui::show_error(self.mtm(), "Could not load settings", &format!("{error:#}"));
                return;
            }
        };
        if let Err(e) = terminal::launch_ssh(&settings, connection) {
            ui::show_error(self.mtm(), "Could not start SSH session", &e.to_string());
        }
    }

    /// Obtain the password: use the saved Keychain entry silently when present,
    /// otherwise prompt. The prompt offers a "remember" checkbox that saves the
    /// password and stops asking on future connects.
    fn resolve_password(&self, mtm: MainThreadMarker, connection: &Connection) -> Option<String> {
        let remember = connection.rdp.password_policy == PasswordPolicy::Remember;
        if remember {
            match secrets::load_password(&connection.id) {
                Ok(Some(p)) => {
                    // Re-create the item so it is owned by the current app
                    // identity. Items written by an earlier build otherwise
                    // carry a stale access list and prompt on every read.
                    if let Err(e) = secrets::store_password(&connection.id, &p) {
                        tracing::warn!("could not refresh keychain item: {e:#}");
                    }
                    return Some(p);
                }
                Ok(None) => {}
                Err(e) => ui::show_error(mtm, "Keychain error", &format!("{e:#}")),
            }
        }

        // Default the checkbox to on for "remember" connections.
        let (password, save) = ui::prompt_password(mtm, &connection.name, remember)?;
        if save {
            if let Err(e) = secrets::store_password(&connection.id, &password) {
                ui::show_error(mtm, "Could not save password", &format!("{e:#}"));
            } else if !remember {
                // The user chose to remember: persist the switch so we stop asking.
                let store = self.ivars().borrow().store.clone();
                if let Err(e) = store.set_password_policy(&connection.id, PasswordPolicy::Remember)
                {
                    tracing::warn!("could not update password policy: {e}");
                }
            }
        }
        Some(password)
    }

    fn open_rdp(&self, connection: Connection) {
        let mtm = self.mtm();

        let password = match connection.rdp.authentication {
            AuthenticationMode::Password => match self.resolve_password(mtm, &connection) {
                Some(password) => password,
                None => return,
            },
            AuthenticationMode::EntraWeb => String::new(),
        };

        let window_id = {
            let mut state = self.ivars().borrow_mut();
            let id = state.next_id;
            state.next_id += 1;
            id
        };

        let controller = WindowController::new(mtm);
        let global_settings = {
            let store = self.ivars().borrow().store.clone();
            store
                .load_document()
                .map(|document| document.settings)
                .unwrap_or_default()
        };
        controller.setup(
            mtm,
            window_id,
            &connection.id,
            &connection.name,
            &connection.rdp,
            global_settings.external_stt_paste,
        );
        controller.set_mac_shortcuts_enabled(global_settings.mac_shortcuts);

        let (width, height, scale) = controller.initial_size();
        let opts = &connection.rdp;
        let config = SessionConfig {
            host: connection.host.clone(),
            port: connection.port,
            username: connection.username.clone(),
            password,
            domain: connection.domain.clone(),
            authentication: opts.authentication,
            allow_tls_without_nla: opts.allow_tls_without_nla,
            width,
            height,
            scale,
            expected_fingerprint: connection.cert_fingerprint.clone(),
            color_depth: opts.color_quality.bits(),
            compression: opts.compression,
            clipboard: opts.clipboard,
            audio: opts.audio,
            graphics: opts.graphics,
            avc444: opts.avc444,
            dynamic_resolution: controller.dynamic_resolution(),
            reconnect: opts.reconnect,
            reconnect_per_minute: opts.reconnect_per_minute,
            swap_cmd_alt: global_settings.swap_cmd_alt,
            wake_mac: opts.wake_mac.clone(),
            keep_alive: opts.keep_alive,
        };

        let frame_pending = Arc::new(AtomicBool::new(false));
        let frame_dirty = Arc::new(AtomicBool::new(false));
        let event_cb: Box<dyn Fn(SessionEvent) + Send> = Box::new(move |event| {
            if matches!(
                event,
                SessionEvent::FrameUpdated { .. } | SessionEvent::Resized { .. }
            ) {
                if frame_pending
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    // Coalesced into the open window; the trailing repaint
                    // will show these pixels.
                    frame_dirty.store(true, Ordering::Release);
                    return;
                }
                let frame_pending = frame_pending.clone();
                let frame_dirty = frame_dirty.clone();
                // Leading paint: repaint immediately so a lone update (typing,
                // cursor feedback) carries no added latency.
                DispatchQueue::main().exec_async(move || deliver(window_id, event));
                // Trailing paint: only when updates were coalesced while the
                // window was open (up to ~120 fps under a sustained stream —
                // ProMotion-friendly — no redundant repaint for a lone update).
                let when = DispatchTime::try_from(Duration::from_millis(8))
                    .expect("8 ms fits in dispatch time");
                let _ = DispatchQueue::main().after(when, move || {
                    frame_pending.store(false, Ordering::Release);
                    if frame_dirty.swap(false, Ordering::AcqRel) {
                        deliver(
                            window_id,
                            SessionEvent::FrameUpdated {
                                x: 0,
                                y: 0,
                                width: 0,
                                height: 0,
                            },
                        );
                    }
                });
            } else {
                DispatchQueue::main().exec_async(move || deliver(window_id, event));
            }
        });
        let handle = spawn_session(config, event_cb);
        controller.attach_session(handle);

        self.ivars()
            .borrow_mut()
            .windows
            .insert(window_id, controller.clone());
        // A session window should appear in the Dock and ⌘-Tab so it can be
        // raised again; the app drops back to menu-bar-only when none are open.
        self.update_activation_policy();
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        controller.bring_to_front();
    }

    /// Show a Dock icon / ⌘-Tab entry while any session window is open, and
    /// revert to an accessory (menu-bar-only) app when the last one closes.
    fn update_activation_policy(&self) {
        let mtm = self.mtm();
        let has_windows = !self.ivars().borrow().windows.is_empty();
        let policy = if has_windows {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
    }

    fn show_settings(&self) {
        let mtm = self.mtm();
        let existing = self.ivars().borrow().settings.clone();
        let controller = match existing {
            Some(c) => c,
            None => {
                let store = self.ivars().borrow().store.clone();
                let c = SettingsController::new(mtm, store);
                self.ivars().borrow_mut().settings = Some(c.clone());
                c
            }
        };
        controller.show(mtm);
        // `activate()` needs macOS 14; the app supports macOS 11.
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    fn handle_session_event(&self, window_id: u64, event: SessionEvent) {
        let mtm = self.mtm();
        let controller = self.ivars().borrow().windows.get(&window_id).cloned();
        let Some(controller) = controller else { return };

        match event {
            SessionEvent::Connected { .. } => {
                controller.end_entra_sign_in();
                controller.set_connected();
                controller.refresh();
            }
            SessionEvent::FrameUpdated { .. } | SessionEvent::Resized { .. } => {
                controller.refresh()
            }
            SessionEvent::PointerBitmap {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => controller.set_pointer_bitmap(rgba, width, height, hotspot_x, hotspot_y),
            SessionEvent::PointerDefault => controller.set_pointer_default(),
            SessionEvent::PointerHidden => controller.set_pointer_hidden(),
            SessionEvent::Reconnecting {
                attempt,
                max_attempts,
            } => controller.set_reconnecting(attempt, max_attempts),
            SessionEvent::ClipboardText(text) => controller.set_clipboard(&text),
            SessionEvent::ClipboardFilesPreparing { count } => {
                controller.prepare_remote_files(count)
            }
            SessionEvent::ClipboardFilesReady(paths) => controller.offer_remote_files(paths),
            SessionEvent::ClipboardFilesFailed(message) => {
                controller.remote_file_preparation_failed(&message)
            }
            SessionEvent::CertificateApproval {
                fingerprint,
                is_change,
                reply,
            } => {
                let ok = ui::prompt_certificate(mtm, &fingerprint, is_change);
                let _ = reply.send(ok);
            }
            SessionEvent::EntraSignIn {
                authorization_url,
                redirect_uri,
                reply,
            } => controller.begin_entra_sign_in(mtm, &authorization_url, redirect_uri, reply),
            SessionEvent::CertTrusted { fingerprint } => {
                let store = self.ivars().borrow().store.clone();
                if let Err(e) = store.set_fingerprint(&controller.connection_id(), &fingerprint) {
                    ui::show_error(mtm, "Could not save server key", &format!("{e:#}"));
                }
            }
            SessionEvent::Disconnected { reason } => {
                controller.end_entra_sign_in();
                match terminal_session_presentation(TerminalSessionKind::Disconnected, &reason) {
                    TerminalSessionPresentation::CloseSilently => controller.close(),
                    TerminalSessionPresentation::SheetAndClose { title, message } => {
                        controller.show_terminal_sheet(mtm, title, message);
                    }
                }
            }
            SessionEvent::ReconnectFailed { reason } => {
                controller.end_entra_sign_in();
                match terminal_session_presentation(TerminalSessionKind::ReconnectFailed, &reason) {
                    TerminalSessionPresentation::CloseSilently => controller.close(),
                    TerminalSessionPresentation::SheetAndClose { title, message } => {
                        controller.show_terminal_sheet(mtm, title, message);
                    }
                }
            }
            SessionEvent::Error(message) => {
                controller.end_entra_sign_in();
                match terminal_session_presentation(TerminalSessionKind::ConnectionError, &message)
                {
                    TerminalSessionPresentation::CloseSilently => controller.close(),
                    TerminalSessionPresentation::SheetAndClose { title, message } => {
                        controller.show_terminal_sheet(mtm, title, message);
                    }
                }
            }
        }
    }
}

/// Install the (invisible) main menu so standard editing key equivalents
/// reach text fields. Items target the first responder (nil target).
fn install_main_menu(mtm: MainThreadMarker) {
    fn item(
        mtm: MainThreadMarker,
        title: &str,
        action: objc2::runtime::Sel,
        key: &str,
    ) -> Retained<NSMenuItem> {
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        }
    }

    let main_menu = NSMenu::new(mtm);

    // App menu (first slot), in the HIG's standard order. Session views
    // answer `hide:`/`hideOtherApplications:` themselves and send the keys
    // to the remote, as they do for ⌘W.
    let app_slot = NSMenuItem::new(mtm);
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&item(
        mtm,
        "About RDP123",
        sel!(orderFrontStandardAboutPanel:),
        "",
    ));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(mtm, "Settings…", sel!(openSettings:), ","));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let services_slot = item(mtm, "Services", sel!(submenuAction:), "");
    let services_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Services"));
    services_slot.setSubmenu(Some(&services_menu));
    app_menu.addItem(&services_slot);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(mtm, "Hide RDP123", sel!(hide:), "h"));
    let hide_others = item(mtm, "Hide Others", sel!(hideOtherApplications:), "h");
    hide_others
        .setKeyEquivalentModifierMask(NSEventModifierFlags::Command | NSEventModifierFlags::Option);
    app_menu.addItem(&hide_others);
    app_menu.addItem(&item(mtm, "Show All", sel!(unhideAllApplications:), ""));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item(mtm, "Quit RDP123", sel!(terminate:), "q"));
    app_slot.setSubmenu(Some(&app_menu));
    main_menu.addItem(&app_slot);
    NSApplication::sharedApplication(mtm).setServicesMenu(Some(&services_menu));

    // File menu: Close ⌘W for the Settings window. Session windows answer
    // `performClose:` themselves and send ⌘W to the remote instead.
    // Duplicate ⌘D reaches the Settings controller as its window's delegate;
    // with any other window key nothing answers it, so ⌘D passes through.
    let file_slot = NSMenuItem::new(mtm);
    let file_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("File"));
    file_menu.addItem(&item(mtm, "Close", sel!(performClose:), "w"));
    file_menu.addItem(&item(mtm, "Duplicate", sel!(duplicateConnection:), "d"));
    file_slot.setSubmenu(Some(&file_menu));
    main_menu.addItem(&file_slot);

    // Edit menu: the standard first-responder actions.
    let edit_slot = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Edit"));
    edit_menu.addItem(&item(mtm, "Undo", sel!(undo:), "z"));
    edit_menu.addItem(&item(mtm, "Redo", sel!(redo:), "Z"));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item(mtm, "Cut", sel!(cut:), "x"));
    edit_menu.addItem(&item(mtm, "Copy", sel!(copy:), "c"));
    edit_menu.addItem(&item(mtm, "Paste", sel!(paste:), "v"));
    edit_menu.addItem(&NSMenuItem::separatorItem(mtm));
    edit_menu.addItem(&item(mtm, "Select All", sel!(selectAll:), "a"));
    edit_slot.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_slot);

    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main_menu));
}

#[cfg(test)]
mod tests {
    use super::{terminal_session_presentation, TerminalSessionKind, TerminalSessionPresentation};

    #[test]
    fn terminal_failures_do_not_leave_a_frozen_session_inline() {
        assert_eq!(
            terminal_session_presentation(TerminalSessionKind::Disconnected, "network dropped"),
            TerminalSessionPresentation::SheetAndClose {
                title: "Disconnected",
                message: "network dropped",
            }
        );
        assert_eq!(
            terminal_session_presentation(TerminalSessionKind::ConnectionError, "host unavailable"),
            TerminalSessionPresentation::SheetAndClose {
                title: "Connection failed",
                message: "host unavailable",
            }
        );
        assert_eq!(
            terminal_session_presentation(TerminalSessionKind::ReconnectFailed, "host unavailable"),
            TerminalSessionPresentation::SheetAndClose {
                title: "Reconnect failed",
                message: "host unavailable",
            }
        );
    }

    #[test]
    fn normal_remote_logoff_closes_without_an_error() {
        assert_eq!(
            terminal_session_presentation(
                TerminalSessionKind::Disconnected,
                rdp123_core::REMOTE_ENDED
            ),
            TerminalSessionPresentation::CloseSilently
        );
    }
}
