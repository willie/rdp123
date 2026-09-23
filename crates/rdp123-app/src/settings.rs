//! The Settings window: manage connections and global settings.
//!
//! Layout: a settings toolbar ("Connections", "Global", "About") switches panes. The
//! Connection pane has the connection list plus a grouped editor; the Global
//! pane has the shared SSH-terminal setting (entered once, used by every SSH
//! connection).
//!
//! Save model: editing a connection stages changes in the form and marks it
//! dirty; nothing is written until **Save** (or the confirm-on-switch dialog).
//! **Revert** re-loads from disk. Global settings are simple and auto-save.
//! Programmatic form population is guarded by `loading` so it never writes back
//! or marks dirty; `updating` guards a programmatic re-selection from recursing.

#![allow(clippy::too_many_arguments)]

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSBorderType, NSBox, NSBoxType,
    NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSControlTextEditingDelegate,
    NSFont, NSGridCell, NSGridCellPlacement, NSGridRow, NSGridRowAlignment, NSGridView, NSImage,
    NSImageView, NSPasteboard, NSPasteboardTypeString, NSPopUpButton, NSScrollView,
    NSSecureTextField, NSStackView, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTextAlignment, NSTextField, NSTextFieldDelegate, NSToolbar,
    NSToolbarDelegate, NSToolbarDisplayMode, NSToolbarItem, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWindowToolbarStyle, NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol,
    NSRange, NSString, NSURL,
};

use rdp123_core::{
    secrets, AudioMode, AuthenticationMode, ClipboardMode, ColorQuality, Connection,
    ConnectionKind, Document, GraphicsMode, PasswordPolicy, ProfileStore, ResolutionMode,
    ScalingLevel, TerminalKind,
};

use crate::ui::{self, UnsavedChoice};

// Popup item orders (index <-> enum).
const COLOR: [ColorQuality; 2] = [ColorQuality::High32, ColorQuality::Medium16];
const CLIP: [ClipboardMode; 4] = [
    ClipboardMode::Bidirectional,
    ClipboardMode::Disabled,
    ClipboardMode::LocalToRemote,
    ClipboardMode::RemoteToLocal,
];
const SCALING: [ScalingLevel; 5] = [
    ScalingLevel::Auto,
    ScalingLevel::Percent100,
    ScalingLevel::Percent140,
    ScalingLevel::Percent180,
    ScalingLevel::Percent200,
];
const AUDIO: [AudioMode; 3] = [
    AudioMode::ThisComputer,
    AudioMode::Never,
    AudioMode::RemoteComputer,
];
const GRAPHICS: [GraphicsMode; 2] = [GraphicsMode::Egfx, GraphicsMode::Classic];
const RESMODE: [ResolutionMode; 2] = [ResolutionMode::FitToWindow, ResolutionMode::Fixed];
const PWPOLICY: [PasswordPolicy; 2] = [PasswordPolicy::Remember, PasswordPolicy::AlwaysAsk];
const AUTHENTICATION: [AuthenticationMode; 2] =
    [AuthenticationMode::Password, AuthenticationMode::EntraWeb];

// Window / layout geometry.
const W: f64 = 720.0;
const CH: f64 = 800.0; // content height (below the toolbar)
/// The About pane's frame math is written for this height; it is shifted up
/// to the top of the taller content area.
const ABOUT_H: f64 = 716.0;
const GRID_TOP: f64 = 20.0; // space above the first grid row
const INDENT: f64 = 20.0; // leading indent of a control that depends on the row above
const FORM_X: f64 = 224.0;
const LABEL_W: f64 = 150.0;
const FIELD_W: f64 = 322.0;
const ROW_H: f64 = 22.0;

/// Toolbar panes: (item identifier, title, SF Symbol).
const PANES: [(&str, &str, &str); 3] = [
    ("connections", "Connections", "desktopcomputer"),
    ("global", "Global", "gearshape"),
    ("about", "About", "info.circle"),
];

fn pane_identifiers() -> Retained<NSArray<NSString>> {
    NSArray::from_retained_slice(&PANES.map(|(id, ..)| NSString::from_str(id)))
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn index_of<T: PartialEq>(items: &[T], value: &T) -> isize {
    items.iter().position(|v| v == value).unwrap_or(0) as isize
}

type Field = RefCell<Option<Retained<NSTextField>>>;
type Secure = RefCell<Option<Retained<NSSecureTextField>>>;
type Popup = RefCell<Option<Retained<NSPopUpButton>>>;
type Check = RefCell<Option<Retained<NSButton>>>;

#[derive(Default)]
pub struct SettingsIvars {
    store: RefCell<Option<ProfileStore>>,
    document: RefCell<Document>,
    selected: Cell<isize>,
    loading: Cell<bool>,
    /// Guards a programmatic table re-selection from recursing into the handler.
    updating: Cell<bool>,
    /// The selected connection has unsaved edits in the form.
    dirty: Cell<bool>,
    built: Cell<bool>,
    window: RefCell<Option<Retained<NSWindow>>>,
    table: RefCell<Option<Retained<NSTableView>>>,
    /// Index into `PANES` of the visible pane; kept across openings so the
    /// window returns to the pane people used last.
    pane: Cell<usize>,
    toolbar: RefCell<Option<Retained<NSToolbar>>>,
    conn_pane: RefCell<Option<Retained<NSScrollView>>>,
    global_pane: RefCell<Option<Retained<NSScrollView>>>,
    about_pane: RefCell<Option<Retained<NSScrollView>>>,
    save_button: Check,
    revert_button: Check,
    remove_button: Check,
    /// Shown in the editor area when no connection is selected.
    empty_label: RefCell<Option<Retained<NSTextField>>>,
    /// Editor rows shared by RDP and SSH (hidden when nothing is selected).
    common_group: RefCell<Vec<Retained<NSGridRow>>>,

    // Common (RDP + SSH)
    name: Field,
    kind: Popup,
    host: Field,
    port: Field,
    user: Field,

    // RDP
    authentication: Popup,
    password: Secure,
    pw_policy: Popup,
    domain: Field,
    color: Popup,
    clipboard: Popup,
    scaling: Popup,
    res_mode: Popup,
    res_w: Field,
    res_h: Field,
    rate: Field,
    wake_mac: Field,
    compression: Check,
    fullscreen: Check,
    remember_size: Check,
    audio: Popup,
    graphics: Popup,
    reconnect: Check,
    keep_alive: Check,

    // Global
    terminal: Popup,
    custom: Field,
    swap_cmd_alt: Check,
    mac_shortcuts: Check,
    external_stt_paste: Check,
    /// Launch-at-login. Not persisted in the document: the checkbox mirrors
    /// the system's `SMAppService` status.
    launch_at_login: Check,

    rdp_group: RefCell<Vec<Retained<NSGridRow>>>,
    password_auth_group: RefCell<Vec<Retained<NSGridRow>>>,
    entra_auth_group: RefCell<Vec<Retained<NSGridRow>>>,
    ssh_group: RefCell<Vec<Retained<NSGridRow>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123SettingsController"]
    #[ivars = SettingsIvars]
    pub struct SettingsController;

    unsafe impl NSObjectProtocol for SettingsController {}

    unsafe impl NSTableViewDataSource for SettingsController {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _t: &NSTableView) -> NSInteger {
            self.ivars().document.borrow().connections.len() as NSInteger
        }

        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn object_value(
            &self,
            _t: &NSTableView,
            _c: &NSTableColumn,
            row: NSInteger,
        ) -> Option<Retained<AnyObject>> {
            self.ivars().document.borrow().connections.get(row as usize).map(|c| {
                let label = match c.kind {
                    ConnectionKind::Rdp => c.name.clone(),
                    ConnectionKind::Ssh => format!("{} (SSH)", c.name),
                };
                let s = NSString::from_str(&label);
                let any: &AnyObject = &s;
                any.retain()
            })
        }
    }

    unsafe impl NSControlTextEditingDelegate for SettingsController {
        #[unsafe(method(controlTextDidEndEditing:))]
        fn text_end(&self, _n: &NSNotification) {
            // Global text saves when editing ends. Connection text is already
            // tracked by controlTextDidChange; marking it here would report a
            // false edit whenever focus moves to another connection.
            if self.pane() == 1 {
                self.save_global();
            }
        }

        // Mark dirty on every keystroke, not only on end-editing, so closing
        // the window mid-edit still triggers the save prompt.
        #[unsafe(method(controlTextDidChange:))]
        fn text_changed(&self, _n: &NSNotification) {
            if self.pane() == 0 {
                self.mark_dirty();
            }
        }
    }

    unsafe impl NSTextFieldDelegate for SettingsController {}

    unsafe impl NSTableViewDelegate for SettingsController {
        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            self.handle_selection_change();
        }
    }

    unsafe impl NSWindowDelegate for SettingsController {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _n: &NSObject) -> bool {
            self.confirm_discard_ok()
        }
    }

    unsafe impl NSToolbarDelegate for SettingsController {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            _will_insert: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.pane_toolbar_item(identifier)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn toolbar_default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn toolbar_allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }

        #[unsafe(method_id(toolbarSelectableItemIdentifiers:))]
        fn toolbar_selectable_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }
    }

    impl SettingsController {
        #[unsafe(method(addConnection:))]
        fn add(&self, _s: Option<&AnyObject>) {
            if !self.confirm_discard_ok() {
                return;
            }
            let mut document = self.ivars().document.borrow().clone();
            let mut connection = Connection::new("New Connection", ConnectionKind::Rdp);
            connection.host = "localhost".to_string();
            document.connections.push(connection);
            let new_index = document.connections.len() as isize - 1;
            if !self.save_document(&document) {
                return;
            }
            *self.ivars().document.borrow_mut() = document;
            self.ivars().selected.set(-1);
            self.reload_table();
            self.select_row(new_index);
            // Put the cursor straight into the name so typing replaces the stub.
            if let (Some(w), Some(name)) = (
                self.ivars().window.borrow().as_ref(),
                self.ivars().name.borrow().as_ref(),
            ) {
                w.makeFirstResponder(Some(name));
            }
        }

        #[unsafe(method(removeConnection:))]
        fn remove(&self, _s: Option<&AnyObject>) {
            if !self.confirm_discard_ok() {
                return;
            }
            let row = self.ivars().selected.get();
            if row < 0 {
                return;
            }
            let Some(connection) = self
                .ivars()
                .document
                .borrow()
                .connections
                .get(row as usize)
                .cloned()
            else {
                return;
            };
            if !ui::confirm_delete(self.mtm(), &connection.name) {
                return;
            }
            let mut document = self.ivars().document.borrow().clone();
            document.connections.remove(row as usize);
            if !self.save_document(&document) {
                return;
            }
            *self.ivars().document.borrow_mut() = document;
            if let Err(error) = secrets::delete_password(&connection.id) {
                ui::show_error(self.mtm(), "Could not remove saved password", &format!("{error:#}"));
            }
            self.ivars().selected.set(-1);
            self.ivars().dirty.set(false);
            self.reload_table();
            let len = self.ivars().document.borrow().connections.len() as isize;
            self.select_row(if len == 0 { -1 } else { row.min(len - 1) });
        }

        #[unsafe(method(saveConnection:))]
        fn save(&self, _s: Option<&AnyObject>) {
            if self.commit_connection() {
                self.ivars().dirty.set(false);
                self.update_dirty_ui();
                self.reload_table();
            }
        }

        #[unsafe(method(revertConnection:))]
        fn revert(&self, _s: Option<&AnyObject>) {
            let row = self.ivars().selected.get();
            self.populate(row);
        }

        #[unsafe(method(markDirty:))]
        fn mark_dirty_action(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
        }

        #[unsafe(method(resModeChanged:))]
        fn res_mode_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.update_fixed_enabled();
        }

        #[unsafe(method(authenticationChanged:))]
        fn authentication_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.update_visibility();
        }

        #[unsafe(method(typeChanged:))]
        fn type_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.sync_default_port();
            self.update_visibility();
        }

        #[unsafe(method(paneChanged:))]
        fn pane_changed(&self, sender: Option<&AnyObject>) {
            let Some(item) = sender.and_then(|s| s.downcast_ref::<NSToolbarItem>()) else {
                return;
            };
            let id = item.itemIdentifier().to_string();
            if let Some(index) = PANES.iter().position(|(pane_id, ..)| *pane_id == id) {
                self.ivars().pane.set(index);
            }
            self.update_visibility();
        }

        #[unsafe(method(globalChanged:))]
        fn global_changed(&self, _s: Option<&AnyObject>) {
            self.save_global();
        }

        #[unsafe(method(loginItemChanged:))]
        fn login_item_changed(&self, _s: Option<&AnyObject>) {
            let enable = self.check_on(&self.ivars().launch_at_login);
            if let Err(error) = crate::login_item::set_enabled(enable) {
                // Revert to the actual system state and surface the error.
                self.set_check(&self.ivars().launch_at_login, crate::login_item::is_enabled());
                ui::show_error(self.mtm(), "Could not update the login item", &error);
            }
        }

        #[unsafe(method(openLibraryLink:))]
        fn open_library_link(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let title: Retained<NSString> = unsafe { msg_send![sender, title] };
            let url = format!("https://crates.io/crates/{}", title);
            if let Some(url) = NSURL::URLWithString(&NSString::from_str(&url)) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(copyVersionInfo:))]
        fn copy_version_info(&self, _s: Option<&AnyObject>) {
            let info = format!(
                "RDP123 {} ({}), built {}",
                env!("CARGO_PKG_VERSION"),
                env!("RDP123_GIT"),
                env!("RDP123_BUILD_TIME"),
            );
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            unsafe {
                pasteboard.setString_forType(&NSString::from_str(&info), NSPasteboardTypeString);
            }
        }
    }
);

impl SettingsController {
    pub fn new(mtm: MainThreadMarker, store: ProfileStore) -> Retained<Self> {
        let ivars = SettingsIvars::default();
        *ivars.store.borrow_mut() = Some(store);
        ivars.selected.set(-1);
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    pub fn show(&self, mtm: MainThreadMarker) {
        if !self.ivars().built.get() {
            self.build(mtm);
            self.ivars().built.set(true);
        }
        // Already open: just bring it forward — a reload would silently discard
        // unsaved edits.
        if let Some(w) = self.ivars().window.borrow().as_ref() {
            if w.isVisible() {
                w.makeKeyAndOrderFront(None);
                return;
            }
        }
        if let Some(store) = self.ivars().store.borrow().as_ref() {
            match store.load_document() {
                Ok(document) => *self.ivars().document.borrow_mut() = document,
                Err(error) => {
                    ui::show_error(
                        mtm,
                        "Could not load connections",
                        &format!("{error:#}\n\nFix or restore:\n{}", store.path().display()),
                    );
                    return;
                }
            }
        }
        self.ivars().selected.set(-1);
        self.ivars().dirty.set(false);
        self.reload_table();
        let first = if self.ivars().document.borrow().connections.is_empty() {
            -1
        } else {
            0
        };
        self.select_row(first);
        self.update_visibility();
        if let Some(w) = self.ivars().window.borrow().as_ref() {
            w.center();
            w.makeKeyAndOrderFront(None);
        }
    }

    fn build(&self, mtm: MainThreadMarker) {
        // Fixed size: the connection list and the About pane are laid out
        // with frame math for this content size. No minimize button: ⌘,
        // reopens settings, so there is no reason to keep it in the Dock.
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, CH),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("content view");

        // ---- pane switch: a settings toolbar ----
        let toolbar = NSToolbar::initWithIdentifier(
            NSToolbar::alloc(mtm),
            &NSString::from_str("RDP123Settings"),
        );
        toolbar.setDelegate(Some(ProtocolObject::from_ref(self)));
        toolbar.setAllowsUserCustomization(false);
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconAndLabel);
        window.setToolbarStyle(NSWindowToolbarStyle::Preference);
        window.setToolbar(Some(&toolbar));
        *self.ivars().toolbar.borrow_mut() = Some(toolbar);

        // ---- panes ----
        let conn_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        conn_scroll.setBorderType(NSBorderType::NoBorder);
        conn_scroll.setHasVerticalScroller(true);
        conn_scroll.setHasHorizontalScroller(true);
        conn_scroll.setAutohidesScrollers(true);
        conn_scroll.setDrawsBackground(false);
        conn_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let global_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        global_scroll.setBorderType(NSBorderType::NoBorder);
        global_scroll.setHasVerticalScroller(true);
        global_scroll.setHasHorizontalScroller(true);
        global_scroll.setAutohidesScrollers(true);
        global_scroll.setDrawsBackground(false);
        global_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let about_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        about_scroll.setBorderType(NSBorderType::NoBorder);
        about_scroll.setHasVerticalScroller(true);
        about_scroll.setAutohidesScrollers(true);
        about_scroll.setDrawsBackground(false);
        about_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let conn = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        let global = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        let about = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        self.build_connection_pane(mtm, &conn);
        self.build_global_pane(mtm, &global);
        self.build_about_pane(mtm, &about);
        conn_scroll.setDocumentView(Some(&conn));
        global_scroll.setDocumentView(Some(&global));
        about_scroll.setDocumentView(Some(&about));
        content.addSubview(&conn_scroll);
        content.addSubview(&global_scroll);
        content.addSubview(&about_scroll);

        *self.ivars().conn_pane.borrow_mut() = Some(conn_scroll);
        *self.ivars().global_pane.borrow_mut() = Some(global_scroll);
        *self.ivars().about_pane.borrow_mut() = Some(about_scroll);
        *self.ivars().window.borrow_mut() = Some(window);
    }

    /// A centered label helper for the About pane.
    fn centered(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: f64,
        h: f64,
        text: &str,
    ) -> Retained<NSTextField> {
        let l = self.label(mtm, parent, rect(60.0, y, W - 120.0, h), text);
        l.setAlignment(NSTextAlignment::Center);
        l
    }

    /// Standard macOS "About" layout: icon, name, version, then the libraries.
    fn build_about_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        // Everything but the bottom license line keeps its distance from the
        // top of the pane.
        let up = CH - ABOUT_H;

        // App icon, centered.
        if let Some(icon) = NSApplication::sharedApplication(mtm).applicationIconImage() {
            let view = NSImageView::imageViewWithImage(&icon, mtm);
            view.setFrame(rect((W - 96.0) / 2.0, up + 584.0, 96.0, 96.0));
            parent.addSubview(&view);
        }

        // Name + version identity block.
        let title = self.centered(mtm, parent, up + 544.0, 32.0, "RDP123");
        title.setFont(Some(&NSFont::boldSystemFontOfSize(26.0)));

        let version = self.centered(
            mtm,
            parent,
            up + 518.0,
            18.0,
            &format!(
                "Version {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("RDP123_GIT")
            ),
        );
        self.muted(&version);
        version.setSelectable(true);

        let built = self.centered(
            mtm,
            parent,
            up + 498.0,
            16.0,
            &format!("Built {}", env!("RDP123_BUILD_TIME")),
        );
        built.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        built.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        built.setSelectable(true);

        let copy = self.button_ret(
            mtm,
            parent,
            rect((W - 150.0) / 2.0, up + 460.0, 150.0, 28.0),
            "Copy Version Info",
            sel!(copyVersionInfo:),
        );
        let _ = copy;

        // Separator line.
        let separator =
            NSBox::initWithFrame(NSBox::alloc(mtm), rect(120.0, up + 444.0, W - 240.0, 1.0));
        separator.setBoxType(NSBoxType::Separator);
        parent.addSubview(&separator);

        // Direct runtime libraries, in two compact columns of crates.io links.
        let header = self.centered(mtm, parent, up + 408.0, 18.0, "Open Source Libraries");
        header.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let note = self.centered(
            mtm,
            parent,
            up + 388.0,
            15.0,
            "Direct runtime dependencies — click a name to view it on crates.io.",
        );
        note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        self.muted(&note);

        let libs: Vec<(&str, &str)> = env!("RDP123_LIBS")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|entry| entry.split_once(' ').unwrap_or((entry, "")))
            .collect();
        let rows = libs.len().div_ceil(2);
        for (i, (name, version)) in libs.iter().enumerate() {
            let col_x = if i < rows { 68.0 } else { 374.0 };
            let y = up + 356.0 - (i % rows) as f64 * 19.0;
            self.link_button(mtm, parent, rect(col_x, y, 176.0, 18.0), name);
            let ver = self.label(mtm, parent, rect(col_x + 182.0, y, 82.0, 18.0), version);
            ver.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            self.muted(&ver);
        }

        // License line pinned to the bottom.
        let license = self.centered(mtm, parent, 28.0, 15.0, "Open source under GNU GPL v3.0");
        license.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        license.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    }

    fn build_connection_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        // ---- connection list ----
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(16.0, 56.0, 190.0, CH - 64.0),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(NSBorderType::BezelBorder);
        let table =
            NSTableView::initWithFrame(NSTableView::alloc(mtm), rect(0.0, 0.0, 188.0, CH - 66.0));
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str("name"),
        );
        column.setWidth(184.0);
        column.setEditable(false);
        unsafe {
            table.addTableColumn(&column);
            table.setHeaderView(None);
            table.setRowHeight(20.0);
            table.setUsesAlternatingRowBackgroundColors(true);
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        scroll.setDocumentView(Some(&table));
        parent.addSubview(&scroll);
        *self.ivars().table.borrow_mut() = Some(table);

        // Small square +/− directly under the list (macOS source-list idiom).
        let _ = self.button_ret(
            mtm,
            parent,
            rect(16.0, 16.0, 36.0, 26.0),
            "+",
            sel!(addConnection:),
        );
        let remove = self.button_ret(
            mtm,
            parent,
            rect(56.0, 16.0, 36.0, 26.0),
            "−",
            sel!(removeConnection:),
        );
        remove.setEnabled(false);
        *self.ivars().remove_button.borrow_mut() = Some(remove);

        let save = self.button_ret(
            mtm,
            parent,
            rect(596.0, 16.0, 108.0, 30.0),
            "Save",
            sel!(saveConnection:),
        );
        // Return key triggers Save (standard default-button behavior).
        save.setKeyEquivalent(&NSString::from_str("\r"));
        let revert = self.button_ret(
            mtm,
            parent,
            rect(480.0, 16.0, 108.0, 30.0),
            "Revert",
            sel!(revertConnection:),
        );
        save.setEnabled(false);
        revert.setEnabled(false);
        *self.ivars().save_button.borrow_mut() = Some(save);
        *self.ivars().revert_button.borrow_mut() = Some(revert);

        let dirty = sel!(markDirty:);

        // Shown when the list is empty / nothing is selected.
        let empty = self.label(
            mtm,
            parent,
            rect(FORM_X, CH / 2.0, 460.0, ROW_H),
            "No connection selected — click + to add one.",
        );
        empty.setAlignment(NSTextAlignment::Center);
        self.muted(&empty);
        *self.ivars().empty_label.borrow_mut() = Some(empty);

        // ---- editor: one grid, rows listed in order ----
        // Every row belongs to a group; `update_visibility` hides whole rows,
        // and a hidden grid row takes no space, so RDP and SSH layouts need
        // no shared coordinates.
        #[derive(Clone, Copy)]
        enum Group {
            Common,
            Rdp,
            PasswordAuth,
            EntraAuth,
            Ssh,
        }
        let label = |text: &str| NSTextField::labelWithString(&NSString::from_str(text), mtm);
        let muted = |text: &str| {
            let l = label(text);
            self.muted(&l);
            l
        };
        let text = |placeholder: &str, width: f64| {
            let t = NSTextField::initWithFrame(NSTextField::alloc(mtm), CGRect::ZERO);
            unsafe { t.setDelegate(Some(ProtocolObject::from_ref(self))) };
            t.setPlaceholderString(Some(&NSString::from_str(placeholder)));
            t.widthAnchor()
                .constraintEqualToConstant(width)
                .setActive(true);
            t
        };
        let popup = |items: &[&str], action: Sel, width: f64| {
            let p = NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                CGRect::ZERO,
                false,
            );
            for it in items {
                p.addItemWithTitle(&NSString::from_str(it));
            }
            unsafe {
                p.setTarget(Some(self.any()));
                p.setAction(Some(action));
            }
            p.widthAnchor()
                .constraintEqualToConstant(width)
                .setActive(true);
            p
        };
        let checkbox = |title: &str| unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(dirty),
                mtm,
            )
        };
        let view = |v: &NSView| -> Retained<NSView> { v.retain() };
        let empty = || NSGridCell::emptyContentView(mtm);
        // (label cell, control cell, group, spans both columns, space above)
        let field = |l: &str, c: &NSView, g| (view(&label(l)), view(c), g, false, 0.0);
        let control = |c: &NSView, g| (empty(), view(c), g, false, 0.0);
        let wide = |v: &NSView, g| (view(v), empty(), g, true, 0.0);
        let heading = |t: &str, g| {
            let l = label(t);
            l.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            (view(&l), empty(), g, true, 12.0)
        };
        // A control that only applies when the one above it is set, indented
        // under it (HIG: indentation conveys hierarchy).
        let dependent = |views: &[Retained<NSView>]| {
            let stack = NSStackView::stackViewWithViews(&NSArray::from_retained_slice(views), mtm);
            stack.setEdgeInsets(NSEdgeInsets {
                top: 0.0,
                left: INDENT,
                bottom: 0.0,
                right: 0.0,
            });
            stack
        };

        let name = text("Office PC", FIELD_W);
        let kind = popup(&["RDP", "SSH"], sel!(typeChanged:), 120.0);
        let host = text("hostname or IP address", FIELD_W);
        let port = text("3389", FIELD_W);
        let authentication = popup(
            &["Password (NLA)", "Microsoft Entra web"],
            sel!(authenticationChanged:),
            210.0,
        );
        let user = text("user or user@company.com", FIELD_W);
        let domain = text("", FIELD_W);
        let password =
            NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), CGRect::ZERO);
        unsafe { password.setDelegate(Some(ProtocolObject::from_ref(self))) };
        password.setPlaceholderString(Some(&NSString::from_str("(unchanged)")));
        password
            .widthAnchor()
            .constraintEqualToConstant(FIELD_W)
            .setActive(true);
        let pw_policy = popup(&["Remember (Keychain)", "Always ask"], dirty, 210.0);
        let res_mode = popup(&["Fit to window", "Fixed"], sel!(resModeChanged:), 160.0);
        let res_w = text("1920", 70.0);
        let res_h = text("1080", 70.0);
        let fixed_size = dependent(&[view(&res_w), view(&label("×")), view(&res_h)]);
        let scaling = popup(&["Auto", "100%", "140%", "180%", "200%"], dirty, 120.0);
        let color = popup(&["High (32-bit)", "Medium (16-bit)"], dirty, 180.0);
        let graphics = popup(
            &["RDP 8.0 (Experimental)", "RDP 6.1 (Classic bitmaps)"],
            dirty,
            240.0,
        );
        // This controls the outer FastPath/XCRUSH transport layer in either
        // graphics mode. EGFX additionally manages ZGFX and codec compression.
        let compression = checkbox("Transport compression (recommended)");
        let fullscreen = checkbox("Start in full screen");
        let remember_size = checkbox("Remember window size");
        let clipboard = popup(
            &[
                "Bidirectional",
                "Disabled",
                "Local → Remote",
                "Remote → Local",
            ],
            dirty,
            200.0,
        );
        let audio = popup(
            &["On this computer", "Never", "On the remote computer"],
            dirty,
            220.0,
        );
        let reconnect = checkbox("Automatically reconnect after connection drops");
        let rate = text("", 60.0);
        let rate_row = dependent(&[view(&label("Max attempts/minute:")), view(&rate)]);
        let keep_alive = checkbox("Keep session awake");
        keep_alive.setToolTip(Some(&NSString::from_str(
            "While idle, taps an invisible key so the remote session is not \
             disconnected or locked. Also keeps the host from auto-locking.",
        )));
        let wake_mac = text("AA:BB:CC:DD:EE:FF (optional)", FIELD_W);

        use Group::*;
        let mut rows = vec![
            heading("Connection", Common),
            field("Name:", &name, Common),
            field("Type:", &kind, Common),
            field("Host:", &host, Common),
            field("Port:", &port, Common),
            heading("Authentication", Rdp),
            heading("SSH", Ssh),
            field("Method:", &authentication, Rdp),
            field("Username:", &user, Common),
            wide(
                &muted("Opens in the terminal chosen under the “Global” tab."),
                Ssh,
            ),
            wide(
                &muted("Authenticates with your SSH keys — no password is handled here."),
                Ssh,
            ),
            field("Domain:", &domain, PasswordAuth),
            field("Password:", &password, PasswordAuth),
            field("Password handling:", &pw_policy, PasswordAuth),
            control(
                &muted("Signs in interactively with your Microsoft account."),
                EntraAuth,
            ),
            control(
                &muted("Hostname required; IP addresses are not supported."),
                EntraAuth,
            ),
            control(
                &muted("No RDP password or domain is stored or sent."),
                EntraAuth,
            ),
            heading("Display", Rdp),
            field("Resolution:", &res_mode, Rdp),
            control(&fixed_size, Rdp),
            field("Scaling:", &scaling, Rdp),
            field("Color quality:", &color, Rdp),
            field("Graphics:", &graphics, Rdp),
            control(&compression, Rdp),
            control(&fullscreen, Rdp),
            control(&remember_size, Rdp),
            heading("Clipboard, sound & session", Rdp),
            field("Clipboard:", &clipboard, Rdp),
            field("Play sound:", &audio, Rdp),
            control(&reconnect, Rdp),
            control(&rate_row, Rdp),
            control(&keep_alive, Rdp),
            field("Wake on LAN (MAC):", &wake_mac, Rdp),
        ];
        rows[0].4 = 0.0; // no gap above the first heading

        let grid_rows: Vec<Retained<NSArray<NSView>>> = rows
            .iter()
            .map(|(l, c, ..)| NSArray::from_retained_slice(&[l.clone(), c.clone()]))
            .collect();
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&grid_rows), mtm);
        grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
        let labels = grid.columnAtIndex(0);
        labels.setXPlacement(NSGridCellPlacement::Trailing);
        labels.setWidth(LABEL_W);
        grid.columnAtIndex(1)
            .setXPlacement(NSGridCellPlacement::Leading);

        let (mut common, mut rdp, mut password_auth, mut entra_auth, mut ssh) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (i, (_, _, group, spans, top)) in rows.iter().enumerate() {
            let index = i as isize;
            let row = grid.rowAtIndex(index);
            row.setTopPadding(*top);
            if *spans {
                grid.mergeCellsInHorizontalRange_verticalRange(
                    NSRange::new(0, 2),
                    NSRange::new(i, 1),
                );
                grid.cellAtColumnIndex_rowIndex(0, index)
                    .setXPlacement(NSGridCellPlacement::Leading);
            }
            match group {
                Common => common.push(row),
                Rdp => rdp.push(row),
                PasswordAuth => {
                    rdp.push(row.clone());
                    password_auth.push(row);
                }
                EntraAuth => {
                    rdp.push(row.clone());
                    entra_auth.push(row);
                }
                Ssh => ssh.push(row),
            }
        }

        grid.setTranslatesAutoresizingMaskIntoConstraints(false);
        parent.addSubview(&grid);
        grid.topAnchor()
            .constraintEqualToAnchor_constant(&parent.topAnchor(), GRID_TOP)
            .setActive(true);
        grid.leadingAnchor()
            .constraintEqualToAnchor_constant(&parent.leadingAnchor(), FORM_X)
            .setActive(true);

        let ivars = self.ivars();
        *ivars.name.borrow_mut() = Some(name);
        *ivars.kind.borrow_mut() = Some(kind);
        *ivars.host.borrow_mut() = Some(host);
        *ivars.port.borrow_mut() = Some(port);
        *ivars.authentication.borrow_mut() = Some(authentication);
        *ivars.user.borrow_mut() = Some(user);
        *ivars.domain.borrow_mut() = Some(domain);
        *ivars.password.borrow_mut() = Some(password);
        *ivars.pw_policy.borrow_mut() = Some(pw_policy);
        *ivars.res_mode.borrow_mut() = Some(res_mode);
        *ivars.res_w.borrow_mut() = Some(res_w);
        *ivars.res_h.borrow_mut() = Some(res_h);
        *ivars.scaling.borrow_mut() = Some(scaling);
        *ivars.color.borrow_mut() = Some(color);
        *ivars.graphics.borrow_mut() = Some(graphics);
        *ivars.compression.borrow_mut() = Some(compression);
        *ivars.fullscreen.borrow_mut() = Some(fullscreen);
        *ivars.remember_size.borrow_mut() = Some(remember_size);
        *ivars.clipboard.borrow_mut() = Some(clipboard);
        *ivars.audio.borrow_mut() = Some(audio);
        *ivars.reconnect.borrow_mut() = Some(reconnect);
        *ivars.rate.borrow_mut() = Some(rate);
        *ivars.keep_alive.borrow_mut() = Some(keep_alive);
        *ivars.wake_mac.borrow_mut() = Some(wake_mac);
        *ivars.common_group.borrow_mut() = common;
        *ivars.rdp_group.borrow_mut() = rdp;
        *ivars.password_auth_group.borrow_mut() = password_auth;
        *ivars.entra_auth_group.borrow_mut() = entra_auth;
        *ivars.ssh_group.borrow_mut() = ssh;
    }

    /// Laid out by an `NSGridView`: a right-aligned label column and a control
    /// column, rows aligned on their first baseline. No frame arithmetic.
    fn build_global_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        let label = |text: &str| NSTextField::labelWithString(&NSString::from_str(text), mtm);
        let muted = |text: &str| {
            let l = label(text);
            self.muted(&l);
            l
        };
        let checkbox = |title: &str, action: Sel| unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(action),
                mtm,
            )
        };
        let view = |v: &NSView| -> Retained<NSView> { v.retain() };
        let empty = || NSGridCell::emptyContentView(mtm);

        let term = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(0.0, 0.0, 240.0, ROW_H + 2.0),
            false,
        );
        for kind in TerminalKind::ALL {
            term.addItemWithTitle(&NSString::from_str(kind.display_name()));
        }
        unsafe {
            term.setTarget(Some(self.any()));
            term.setAction(Some(sel!(globalChanged:)));
        }

        let custom = NSTextField::initWithFrame(NSTextField::alloc(mtm), CGRect::ZERO);
        unsafe { custom.setDelegate(Some(ProtocolObject::from_ref(self))) };
        custom.setPlaceholderString(Some(&NSString::from_str("e.g. wezterm start -- {ssh}")));
        custom
            .widthAnchor()
            .constraintEqualToConstant(440.0)
            .setActive(true);

        let swap = checkbox(
            "Swap ⌘ and ⌥ in RDP sessions (⌘ acts as Alt, ⌥ as the Windows key)",
            sel!(globalChanged:),
        );
        let mac_shortcuts = checkbox(
            "Use Mac shortcuts in RDP sessions (⌘C, ⌘V, ⌘X, ⌘A, ⌘Z, ⌘F, ⌘W send Ctrl)",
            sel!(globalChanged:),
        );
        let external_stt_paste = checkbox(
            "Enable external STT paste in RDP sessions",
            sel!(globalChanged:),
        );
        let login = checkbox(
            "Start RDP123 automatically when you log in",
            sel!(loginItemChanged:),
        );

        // (row, extra space above it)
        let mut rows: Vec<(Vec<Retained<NSView>>, f64)> = vec![
            (vec![view(&label("SSH terminal:")), view(&term)], 0.0),
            (vec![view(&label("Custom command:")), view(&custom)], 0.0),
            (
                vec![
                    empty(),
                    view(&label(
                        "Custom command is used only for the “Custom command…” terminal.",
                    )),
                ],
                0.0,
            ),
            (
                vec![
                    empty(),
                    view(&label(
                        "Placeholders: {ssh} = the full ssh command; {host}, {port}, {user} are also available.",
                    )),
                ],
                0.0,
            ),
            (
                vec![
                    empty(),
                    view(&label("This terminal is shared by every SSH connection.")),
                ],
                0.0,
            ),
            (vec![view(&label("Keyboard:")), view(&swap)], 16.0),
            (
                vec![
                    empty(),
                    view(&label(
                        "Matches the PC key layout: the key next to the space bar is Alt.",
                    )),
                ],
                0.0,
            ),
            (vec![empty(), view(&mac_shortcuts)], 0.0),
            (vec![view(&label("Speech to text:")), view(&external_stt_paste)], 16.0),
            (
                vec![
                    empty(),
                    view(&muted(
                        "Synchronizes macOS clipboard text before inserting it remotely with Ctrl+V.",
                    )),
                ],
                0.0,
            ),
            (vec![view(&label("Startup:")), view(&login)], 16.0),
        ];
        if !crate::login_item::is_supported() {
            login.setEnabled(false);
            rows.push((
                vec![empty(), view(&muted("Requires macOS 13 or newer."))],
                0.0,
            ));
        }

        let grid_rows: Vec<Retained<NSArray<NSView>>> = rows
            .iter()
            .map(|(cells, _)| NSArray::from_retained_slice(cells))
            .collect();
        let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&grid_rows), mtm);
        grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
        grid.columnAtIndex(0)
            .setXPlacement(NSGridCellPlacement::Trailing);
        grid.columnAtIndex(1)
            .setXPlacement(NSGridCellPlacement::Leading);
        for (i, (_, padding)) in rows.iter().enumerate() {
            grid.rowAtIndex(i as isize).setTopPadding(*padding);
        }

        grid.setTranslatesAutoresizingMaskIntoConstraints(false);
        parent.addSubview(&grid);
        grid.topAnchor()
            .constraintEqualToAnchor_constant(&parent.topAnchor(), GRID_TOP)
            .setActive(true);
        grid.leadingAnchor()
            .constraintEqualToAnchor_constant(&parent.leadingAnchor(), 32.0)
            .setActive(true);

        *self.ivars().terminal.borrow_mut() = Some(term);
        *self.ivars().custom.borrow_mut() = Some(custom);
        *self.ivars().swap_cmd_alt.borrow_mut() = Some(swap);
        *self.ivars().mac_shortcuts.borrow_mut() = Some(mac_shortcuts);
        *self.ivars().external_stt_paste.borrow_mut() = Some(external_stt_paste);
        *self.ivars().launch_at_login.borrow_mut() = Some(login);
    }

    // ---------- small control builders ----------

    fn label(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        text: &str,
    ) -> Retained<NSTextField> {
        let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        l.setFrame(f);
        parent.addSubview(&l);
        l
    }

    /// Colour a label as secondary/muted text.
    fn muted(&self, label: &NSTextField) {
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }

    /// A borderless, link-coloured button whose title opens a crates.io page.
    fn link_button(&self, mtm: MainThreadMarker, parent: &NSView, f: CGRect, title: &str) {
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(sel!(openLibraryLink:)),
                mtm,
            )
        };
        b.setFrame(f);
        b.setBordered(false);
        b.setAlignment(NSTextAlignment::Left);
        b.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        b.setContentTintColor(Some(&NSColor::linkColor()));
        parent.addSubview(&b);
    }

    fn button_ret(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        title: &str,
        action: Sel,
    ) -> Retained<NSButton> {
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(action),
                mtm,
            )
        };
        b.setFrame(f);
        parent.addSubview(&b);
        b
    }

    fn any(&self) -> &AnyObject {
        self
    }

    // ---------- data flow ----------

    fn pane_toolbar_item(&self, identifier: &NSString) -> Option<Retained<NSToolbarItem>> {
        let id = identifier.to_string();
        let (_, title, symbol) = PANES.iter().find(|(pane_id, ..)| *pane_id == id)?;
        let item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), identifier);
        let title = NSString::from_str(title);
        item.setLabel(&title);
        item.setImage(
            NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                Some(&title),
            )
            .as_deref(),
        );
        unsafe {
            item.setTarget(Some(self.any()));
            item.setAction(Some(sel!(paneChanged:)));
        }
        Some(item)
    }

    fn pane(&self) -> usize {
        self.ivars().pane.get()
    }

    fn save_document(&self, document: &Document) -> bool {
        let Some(store) = self.ivars().store.borrow().as_ref().cloned() else {
            ui::show_error(
                self.mtm(),
                "Could not save settings",
                "The profile store is unavailable.",
            );
            return false;
        };
        match store.save_document(document) {
            Ok(()) => true,
            Err(error) => {
                ui::show_error(self.mtm(), "Could not save settings", &format!("{error:#}"));
                false
            }
        }
    }

    fn reload_table(&self) {
        if let Some(t) = self.ivars().table.borrow().as_ref() {
            t.reloadData();
        }
    }

    fn select_row(&self, row: isize) {
        let len = self.ivars().document.borrow().connections.len() as isize;
        if row >= 0 && row < len {
            if let Some(t) = self.ivars().table.borrow().as_ref() {
                self.ivars().updating.set(true);
                let set = NSIndexSet::indexSetWithIndex(row as usize);
                t.selectRowIndexes_byExtendingSelection(&set, false);
                self.ivars().updating.set(false);
            }
            self.populate(row);
        } else {
            if let Some(t) = self.ivars().table.borrow().as_ref() {
                unsafe { t.deselectAll(None) };
            }
            self.populate(-1);
        }
    }

    fn handle_selection_change(&self) {
        if self.ivars().updating.get() {
            return;
        }
        let target = self
            .ivars()
            .table
            .borrow()
            .as_ref()
            .map(|t| t.selectedRow())
            .unwrap_or(-1);
        let prev = self.ivars().selected.get();
        if target == prev {
            return;
        }
        if self.ivars().dirty.get() && prev >= 0 {
            let name = self
                .ivars()
                .document
                .borrow()
                .connections
                .get(prev as usize)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            match ui::confirm_unsaved(self.mtm(), &name) {
                UnsavedChoice::Save => {
                    if !self.commit_connection() {
                        self.restore_selection(prev);
                        return;
                    }
                }
                UnsavedChoice::Discard => {}
                UnsavedChoice::Cancel => {
                    self.restore_selection(prev);
                    return;
                }
            }
        }
        self.populate(target);
    }

    /// Returns true if it is OK to proceed (discard/save handled), false to abort.
    fn confirm_discard_ok(&self) -> bool {
        if !self.ivars().dirty.get() || self.ivars().selected.get() < 0 {
            return true;
        }
        let name = self
            .ivars()
            .document
            .borrow()
            .connections
            .get(self.ivars().selected.get() as usize)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        match ui::confirm_unsaved(self.mtm(), &name) {
            UnsavedChoice::Save => {
                if self.commit_connection() {
                    self.ivars().dirty.set(false);
                    self.update_dirty_ui();
                    true
                } else {
                    false
                }
            }
            UnsavedChoice::Discard => {
                self.ivars().dirty.set(false);
                self.update_dirty_ui();
                true
            }
            UnsavedChoice::Cancel => false,
        }
    }

    fn mark_dirty(&self) {
        // Nothing selected means nothing to stage — don't arm a Save that would
        // silently no-op.
        if self.ivars().loading.get() || self.pane() != 0 || self.ivars().selected.get() < 0 {
            return;
        }
        self.ivars().dirty.set(true);
        self.update_dirty_ui();
    }

    fn update_dirty_ui(&self) {
        let dirty = self.ivars().dirty.get();
        if let Some(b) = self.ivars().save_button.borrow().as_ref() {
            b.setEnabled(dirty);
        }
        if let Some(b) = self.ivars().revert_button.borrow().as_ref() {
            b.setEnabled(dirty);
        }
    }

    fn update_visibility(&self) {
        let pane = self.pane();
        if let Some(c) = self.ivars().conn_pane.borrow().as_ref() {
            c.setHidden(pane != 0);
        }
        if let Some(g) = self.ivars().global_pane.borrow().as_ref() {
            g.setHidden(pane != 1);
        }
        if let Some(a) = self.ivars().about_pane.borrow().as_ref() {
            a.setHidden(pane != 2);
        }
        let (id, title, _) = PANES[pane];
        if let Some(w) = self.ivars().window.borrow().as_ref() {
            w.setTitle(&NSString::from_str(title));
        }
        if let Some(t) = self.ivars().toolbar.borrow().as_ref() {
            t.setSelectedItemIdentifier(Some(&NSString::from_str(id)));
        }
        let has_selection = self.ivars().selected.get() >= 0;
        let is_ssh = self
            .ivars()
            .kind
            .borrow()
            .as_ref()
            .map(|k| k.indexOfSelectedItem() == 1)
            .unwrap_or(false);
        for v in self.ivars().common_group.borrow().iter() {
            v.setHidden(!has_selection);
        }
        for v in self.ivars().rdp_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh);
        }
        for v in self.ivars().ssh_group.borrow().iter() {
            v.setHidden(!has_selection || !is_ssh);
        }
        if let Some(e) = self.ivars().empty_label.borrow().as_ref() {
            e.setHidden(has_selection);
        }
        if let Some(b) = self.ivars().remove_button.borrow().as_ref() {
            b.setEnabled(has_selection);
        }
        let entra = self.popup_index(&self.ivars().authentication) == 1;
        for v in self.ivars().password_auth_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh || entra);
        }
        for v in self.ivars().entra_auth_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh || !entra);
        }
    }

    fn update_fixed_enabled(&self) {
        let fixed = self.popup_index(&self.ivars().res_mode) == 1;
        if let Some(f) = self.ivars().res_w.borrow().as_ref() {
            f.setEnabled(fixed);
        }
        if let Some(f) = self.ivars().res_h.borrow().as_ref() {
            f.setEnabled(fixed);
        }
    }

    fn sync_default_port(&self) {
        let is_ssh = self.popup_index(&self.ivars().kind) == 1;
        let port = self.read_field(&self.ivars().port);
        if let Ok(p) = port.trim().parse::<u16>() {
            if is_ssh && p == 3389 {
                self.set_field(&self.ivars().port, "22");
            } else if !is_ssh && p == 22 {
                self.set_field(&self.ivars().port, "3389");
            }
        }
    }

    fn set_field(&self, field: &Field, value: &str) {
        if let Some(f) = field.borrow().as_ref() {
            f.setStringValue(&NSString::from_str(value));
        }
    }

    fn read_field(&self, field: &Field) -> String {
        field
            .borrow()
            .as_ref()
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default()
    }

    fn set_popup(&self, popup: &Popup, index: isize) {
        if let Some(p) = popup.borrow().as_ref() {
            p.selectItemAtIndex(index);
        }
    }

    fn popup_index(&self, popup: &Popup) -> isize {
        popup
            .borrow()
            .as_ref()
            .map(|p| p.indexOfSelectedItem())
            .unwrap_or(0)
    }

    fn set_check(&self, check: &Check, on: bool) {
        if let Some(c) = check.borrow().as_ref() {
            c.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
    }

    fn check_on(&self, check: &Check) -> bool {
        check
            .borrow()
            .as_ref()
            .map(|c| c.state() == NSControlStateValueOn)
            .unwrap_or(false)
    }

    fn read_secure(&self, field: &Secure) -> String {
        field
            .borrow()
            .as_ref()
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default()
    }

    fn set_secure(&self, field: &Secure, value: &str) {
        if let Some(f) = field.borrow().as_ref() {
            f.setStringValue(&NSString::from_str(value));
        }
    }

    fn populate(&self, row: isize) {
        self.ivars().loading.set(true);
        let iv = self.ivars();

        // Global settings.
        let (term_idx, custom) = {
            let doc = iv.document.borrow();
            let idx = TerminalKind::ALL
                .iter()
                .position(|k| *k == doc.settings.terminal)
                .unwrap_or(0);
            (
                idx as isize,
                doc.settings.custom_terminal.clone().unwrap_or_default(),
            )
        };
        self.set_popup(&iv.terminal, term_idx);
        self.set_field(&iv.custom, &custom);
        let swap_cmd_alt = iv.document.borrow().settings.swap_cmd_alt;
        self.set_check(&iv.swap_cmd_alt, swap_cmd_alt);
        let mac_shortcuts = iv.document.borrow().settings.mac_shortcuts;
        self.set_check(&iv.mac_shortcuts, mac_shortcuts);
        let external_stt_paste = iv.document.borrow().settings.external_stt_paste;
        self.set_check(&iv.external_stt_paste, external_stt_paste);
        // Reflect the system's login-item state, not a stored flag.
        self.set_check(&iv.launch_at_login, crate::login_item::is_enabled());

        let conn = if row >= 0 {
            iv.document.borrow().connections.get(row as usize).cloned()
        } else {
            None
        };

        if let Some(c) = conn {
            self.set_field(&iv.name, &c.name);
            self.set_field(&iv.host, &c.host);
            self.set_field(&iv.port, &c.port.to_string());
            self.set_field(&iv.user, &c.username);
            self.set_popup(
                &iv.kind,
                match c.kind {
                    ConnectionKind::Rdp => 0,
                    ConnectionKind::Ssh => 1,
                },
            );
            self.set_secure(&iv.password, "");
            self.set_popup(
                &iv.authentication,
                index_of(&AUTHENTICATION, &c.rdp.authentication),
            );
            self.set_popup(&iv.pw_policy, index_of(&PWPOLICY, &c.rdp.password_policy));
            self.set_field(&iv.domain, c.domain.as_deref().unwrap_or(""));
            self.set_popup(&iv.color, index_of(&COLOR, &c.rdp.color_quality));
            self.set_popup(&iv.clipboard, index_of(&CLIP, &c.rdp.clipboard));
            self.set_popup(&iv.scaling, index_of(&SCALING, &c.rdp.scaling));
            self.set_popup(&iv.res_mode, index_of(&RESMODE, &c.rdp.resolution_mode));
            let (rw, rh) = c.rdp.resolution.unwrap_or((1920, 1080));
            self.set_field(&iv.res_w, &rw.to_string());
            self.set_field(&iv.res_h, &rh.to_string());
            self.set_field(&iv.rate, &c.rdp.reconnect_per_minute.to_string());
            self.set_field(&iv.wake_mac, c.rdp.wake_mac.as_deref().unwrap_or(""));
            self.set_check(&iv.compression, c.rdp.compression);
            self.set_check(&iv.fullscreen, c.rdp.fullscreen);
            self.set_check(&iv.remember_size, c.rdp.remember_size);
            self.set_popup(&iv.audio, index_of(&AUDIO, &c.rdp.audio));
            self.set_popup(&iv.graphics, index_of(&GRAPHICS, &c.rdp.graphics));
            self.set_check(&iv.reconnect, c.rdp.reconnect);
            self.set_check(&iv.keep_alive, c.rdp.keep_alive);
        } else {
            for f in [&iv.name, &iv.host, &iv.port, &iv.user, &iv.domain] {
                self.set_field(f, "");
            }
            self.set_secure(&iv.password, "");
        }

        iv.selected.set(row);
        iv.dirty.set(false);
        iv.loading.set(false);
        self.update_dirty_ui();
        self.update_visibility();
        self.update_fixed_enabled();
    }

    /// Write the form into the selected connection and persist (called on Save).
    fn commit_connection(&self) -> bool {
        let row = self.ivars().selected.get();
        if row < 0 {
            return false;
        }
        let iv = self.ivars();
        let name = self.read_field(&iv.name).trim().to_string();
        let host = self.read_field(&iv.host).trim().to_string();
        let port = match parse_number::<u16>("Port", &self.read_field(&iv.port)) {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                ui::show_error(
                    self.mtm(),
                    "Invalid connection",
                    "Port must be between 1 and 65535.",
                );
                return false;
            }
            Err(message) => {
                ui::show_error(self.mtm(), "Invalid connection", &message);
                return false;
            }
        };
        let user = self.read_field(&iv.user).trim().to_string();
        let domain = self.read_field(&iv.domain).trim().to_string();
        let res_w = self.read_field(&iv.res_w);
        let res_h = self.read_field(&iv.res_h);
        let rate =
            match parse_number::<u32>("Maximum attempts per minute", &self.read_field(&iv.rate)) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
        let wake_mac = {
            let raw = self.read_field(&iv.wake_mac).trim().to_string();
            if raw.is_empty() {
                None
            } else if rdp123_core::wol::parse_mac(&raw).is_some() {
                Some(raw)
            } else {
                ui::show_error(
                    self.mtm(),
                    "Invalid connection",
                    "The Wake-on-LAN MAC address must look like AA:BB:CC:DD:EE:FF.",
                );
                return false;
            }
        };
        let kind = if self.popup_index(&iv.kind) == 1 {
            ConnectionKind::Ssh
        } else {
            ConnectionKind::Rdp
        };
        let color = COLOR[self.popup_index(&iv.color).clamp(0, 1) as usize];
        let clip = CLIP[self.popup_index(&iv.clipboard).clamp(0, 3) as usize];
        let scaling = SCALING[self.popup_index(&iv.scaling).clamp(0, 4) as usize];
        let res_mode = RESMODE[self.popup_index(&iv.res_mode).clamp(0, 1) as usize];
        let pw_policy = PWPOLICY[self.popup_index(&iv.pw_policy).clamp(0, 1) as usize];
        let authentication =
            AUTHENTICATION[self.popup_index(&iv.authentication).clamp(0, 1) as usize];
        let compression = self.check_on(&iv.compression);
        let fullscreen = self.check_on(&iv.fullscreen);
        let remember_size = self.check_on(&iv.remember_size);
        let audio = AUDIO[self.popup_index(&iv.audio).clamp(0, 2) as usize];
        let graphics = GRAPHICS[self.popup_index(&iv.graphics).clamp(0, 1) as usize];
        let reconnect = self.check_on(&iv.reconnect);
        let keep_alive = self.check_on(&iv.keep_alive);
        let password = self.read_secure(&iv.password);

        let mut document = iv.document.borrow().clone();
        let Some(connection) = document.connections.get_mut(row as usize) else {
            ui::show_error(
                self.mtm(),
                "Could not save connection",
                "The selected connection no longer exists.",
            );
            return false;
        };
        connection.name = name;
        connection.kind = kind;
        connection.host = host;
        connection.port = port;
        connection.username = user;
        connection.domain = if authentication == AuthenticationMode::EntraWeb || domain.is_empty() {
            None
        } else {
            Some(domain)
        };
        connection.rdp.color_quality = color;
        connection.rdp.authentication = authentication;
        connection.rdp.clipboard = clip;
        connection.rdp.scaling = scaling;
        connection.rdp.resolution_mode = res_mode;
        connection.rdp.resolution = if res_mode == ResolutionMode::Fixed {
            let width = match parse_number::<u16>("Fixed width", &res_w) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
            let height = match parse_number::<u16>("Fixed height", &res_h) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
            Some((width, height))
        } else {
            None
        };
        connection.rdp.reconnect_per_minute = rate;
        connection.rdp.wake_mac = wake_mac;
        connection.rdp.compression = compression;
        connection.rdp.fullscreen = fullscreen;
        connection.rdp.remember_size = remember_size;
        connection.rdp.audio = audio;
        connection.rdp.graphics = graphics;
        connection.rdp.reconnect = reconnect;
        connection.rdp.keep_alive = keep_alive;
        connection.rdp.password_policy = pw_policy;
        if let Err(error) = connection.validate() {
            ui::show_error(self.mtm(), "Invalid connection", &format!("{error:#}"));
            return false;
        }
        let connection_id = connection.id.clone();

        if authentication == AuthenticationMode::Password
            && pw_policy == PasswordPolicy::Remember
            && !password.is_empty()
        {
            if let Err(error) = secrets::store_password(&connection_id, &password) {
                ui::show_error(self.mtm(), "Could not save password", &format!("{error:#}"));
                return false;
            }
        }

        if !self.save_document(&document) {
            return false;
        }
        *iv.document.borrow_mut() = document;
        self.set_secure(&iv.password, "");

        if authentication == AuthenticationMode::EntraWeb || pw_policy == PasswordPolicy::AlwaysAsk
        {
            if let Err(error) = secrets::delete_password(&connection_id) {
                ui::show_error(
                    self.mtm(),
                    "Could not remove saved password",
                    &format!("{error:#}"),
                );
            }
        }
        true
    }

    /// Global settings are simple and auto-save on change.
    fn save_global(&self) {
        if self.ivars().loading.get() {
            return;
        }
        let mut document = self.ivars().document.borrow().clone();
        let ti = self.popup_index(&self.ivars().terminal).max(0) as usize;
        document.settings.terminal = TerminalKind::ALL.get(ti).copied().unwrap_or_default();
        let custom = self.read_field(&self.ivars().custom);
        document.settings.custom_terminal = if custom.trim().is_empty() {
            None
        } else {
            Some(custom)
        };
        document.settings.swap_cmd_alt = self.check_on(&self.ivars().swap_cmd_alt);
        document.settings.mac_shortcuts = self.check_on(&self.ivars().mac_shortcuts);
        document.settings.external_stt_paste = self.check_on(&self.ivars().external_stt_paste);
        if self.save_document(&document) {
            crate::delegate::set_mac_shortcuts_enabled(document.settings.mac_shortcuts);
            crate::delegate::set_external_stt_paste_enabled(document.settings.external_stt_paste);
            *self.ivars().document.borrow_mut() = document;
        }
    }

    fn restore_selection(&self, row: isize) {
        if row < 0 {
            return;
        }
        if let Some(table) = self.ivars().table.borrow().as_ref() {
            self.ivars().updating.set(true);
            let set = NSIndexSet::indexSetWithIndex(row as usize);
            table.selectRowIndexes_byExtendingSelection(&set, false);
            self.ivars().updating.set(false);
        }
    }
}

fn parse_number<T>(label: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label} must be a valid number."))
}
