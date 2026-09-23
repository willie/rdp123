//! The custom `NSView` that renders the remote framebuffer and forwards input.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSCursor, NSEvent, NSEventType, NSMenuItem, NSMenuItemValidation, NSPasteboard,
    NSPasteboardTypeString, NSView,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect};

use rdp123_core::{InputEvent, PointerButton, SessionCommand, SessionHandle, SharedFramebuffer};

use crate::ui;

#[derive(Default)]
pub struct RdpViewIvars {
    handle: RefCell<Option<SessionHandle>>,
    framebuffer: RefCell<Option<Arc<SharedFramebuffer>>>,
    input_routing: RefCell<InputRoutingState>,
    /// The server-provided pointer shape; `None` shows the native arrow.
    remote_cursor: RefCell<Option<Retained<NSCursor>>>,
    /// Opt-in bridge for external macOS speech-to-text tools that insert by
    /// invoking the focused application's standard Paste action.
    external_stt_paste_enabled: Cell<bool>,
    /// Recycled presentation buffers (see `ui::upload_framebuffer`).
    present_pool: ui::PresentPool,
    /// Recycled IOSurfaces for zero-copy presentation.
    surface_pool: ui::SurfacePool,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123RdpView"]
    #[ivars = RdpViewIvars]
    pub struct RdpView;

    unsafe impl NSObjectProtocol for RdpView {}

    unsafe impl NSMenuItemValidation for RdpView {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            let action = item.action();
            if action == Some(sel!(paste:)) {
                if self.ivars().external_stt_paste_enabled.get() {
                    self.external_stt_pasteboard_text().is_some()
                } else {
                    self.command_modifier_pressed()
                }
            } else {
                self.command_modifier_pressed()
                    && matches!(
                        action,
                        Some(action)
                            if action == sel!(undo:)
                                || action == sel!(redo:)
                                || action == sel!(cut:)
                                || action == sel!(copy:)
                                || action == sel!(selectAll:)
                                || action == sel!(performClose:)
                    )
            }
        }
    }

    impl RdpView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.forward_key_down(event.keyCode(), key_character(event));
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.forward_key_up(event.keyCode());
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let keycode = event.keyCode();
            let Some((device_mask, family_mask)) = modifier_masks(keycode) else {
                return;
            };
            let flags = event.modifierFlags().0;
            let mut routing = self.ivars().input_routing.borrow_mut();
            let down = if flags & DEVICE_MODIFIER_MASKS != 0 {
                flags & device_mask != 0
            } else if flags & family_mask == 0 {
                false
            } else {
                !routing.is_pressed(keycode)
            };
            let events = routing.modifier_changed(
                keycode,
                down,
                self.ivars().external_stt_paste_enabled.get(),
            );
            drop(routing);
            self.send(events);
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            if !self.ivars().external_stt_paste_enabled.get() {
                self.forward_menu_shortcut(V_KEYCODE);
                return;
            }
            if self.external_stt_pasteboard_text().is_none() {
                return;
            }
            let (events, submit) = self
                .ivars()
                .input_routing
                .borrow_mut()
                .paste_invoked(true);
            self.send(events);
            if submit {
                self.submit_external_stt_paste();
            }
        }

        #[unsafe(method(undo:))]
        fn undo(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(Z_KEYCODE);
        }

        #[unsafe(method(redo:))]
        fn redo(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(Z_KEYCODE);
        }

        #[unsafe(method(cut:))]
        fn cut(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(X_KEYCODE);
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(C_KEYCODE);
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(A_KEYCODE);
        }

        /// File ▸ Close (⌘W) reaches the session view before the window, so
        /// ⌘W goes to the remote (Ctrl+W with Mac shortcuts) rather than
        /// closing the session. The window's close button still closes it.
        #[unsafe(method(performClose:))]
        fn perform_close(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut(W_KEYCODE);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Left, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Left, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Right, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Right, false);
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.mouse_button(event, PointerButton::Middle, true);
            }
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.mouse_button(event, PointerButton::Middle, false);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = self.ivars().remote_cursor.borrow();
            let cursor = cursor.clone().unwrap_or_else(NSCursor::arrowCursor);
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let precise = event.hasPreciseScrollingDeltas();
            let factor = if precise { 4.0 } else { 120.0 };
            let dy = event.scrollingDeltaY();
            let dx = event.scrollingDeltaX();
            let mut events = Vec::new();
            let vy = (dy * factor) as i16;
            if vy != 0 {
                events.push(InputEvent::Wheel { delta: vy, horizontal: false });
            }
            let vx = (dx * factor) as i16;
            if vx != 0 {
                events.push(InputEvent::Wheel { delta: vx, horizontal: true });
            }
            if !events.is_empty() {
                self.send(events);
            }
        }

    }
);

impl RdpView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RdpViewIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Attach the session once it has been spawned.
    pub fn set_session(&self, handle: SessionHandle) {
        *self.ivars().framebuffer.borrow_mut() = Some(handle.framebuffer());
        *self.ivars().handle.borrow_mut() = Some(handle);
    }

    pub fn set_external_stt_paste_enabled(&self, enabled: bool) {
        self.ivars().external_stt_paste_enabled.set(enabled);
    }

    pub fn set_mac_shortcuts_enabled(&self, enabled: bool) {
        self.ivars().input_routing.borrow_mut().mac_shortcuts = enabled;
    }

    pub fn forward_key_down(&self, keycode: u16, character: Option<char>) {
        let (events, external_paste) = self.ivars().input_routing.borrow_mut().key_down(
            keycode,
            character,
            self.ivars().external_stt_paste_enabled.get(),
        );
        self.send(events);
        if external_paste {
            self.submit_external_stt_paste();
        }
    }

    pub fn forward_key_up(&self, keycode: u16) {
        let events = self
            .ivars()
            .input_routing
            .borrow_mut()
            .key_up(keycode, self.ivars().external_stt_paste_enabled.get());
        self.send(events);
    }

    pub fn forward_key_pulse(&self, keycode: u16, character: Option<char>) {
        let (events, external_paste) = self.ivars().input_routing.borrow_mut().key_pulse(
            keycode,
            character,
            self.ivars().external_stt_paste_enabled.get(),
        );
        self.send(events);
        if external_paste {
            self.submit_external_stt_paste();
        }
    }

    fn command_modifier_pressed(&self) -> bool {
        self.ivars()
            .input_routing
            .borrow()
            .pressed_modifiers
            .iter()
            .any(|keycode| is_command_key(*keycode))
    }

    /// A menu key equivalent forwards the key actually pressed, so the layout
    /// decides the shortcut; a menu click falls back to the US key position.
    fn forward_menu_shortcut(&self, keycode: u16) {
        if !self.command_modifier_pressed() {
            return;
        }
        match NSApplication::sharedApplication(self.mtm()).currentEvent() {
            Some(event) if event.r#type() == NSEventType::KeyDown => {
                self.forward_key_pulse(event.keyCode(), key_character(&event))
            }
            _ => self.forward_key_pulse(keycode, None),
        }
    }

    fn external_stt_pasteboard_text(&self) -> Option<String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
        let text = text.as_ref().map(|value| value.to_string());
        external_stt_paste_text(
            self.ivars().external_stt_paste_enabled.get(),
            text.as_deref(),
        )
    }

    fn submit_external_stt_paste(&self) {
        let Some(text) = self.external_stt_pasteboard_text() else {
            return;
        };
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::PasteLocalClipboard(text));
        }
    }

    /// Apply a new server pointer shape (straight-alpha RGBA, remote pixels).
    pub fn set_pointer_bitmap(
        &self,
        rgba: Vec<u8>,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
    ) {
        // Remote pixels -> view points, so the cursor matches the scale the
        // desktop is displayed at.
        let point_scale = match self.ivars().framebuffer.borrow().as_ref() {
            Some(fb) => {
                let (fb_w, _) = fb.dimensions();
                let bounds = self.bounds();
                if fb_w > 0 && bounds.size.width > 0.0 {
                    bounds.size.width / f64::from(fb_w)
                } else {
                    1.0
                }
            }
            None => 1.0,
        };
        let cursor = ui::make_remote_cursor(rgba, width, height, hotspot_x, hotspot_y, point_scale);
        self.apply_cursor(cursor);
    }

    /// Revert to the native arrow pointer.
    pub fn set_pointer_default(&self) {
        self.apply_cursor(None);
    }

    /// Hide the pointer over the remote desktop (transparent cursor, so it
    /// reappears as soon as it leaves the view).
    pub fn set_pointer_hidden(&self) {
        self.apply_cursor(ui::make_hidden_cursor());
    }

    fn apply_cursor(&self, cursor: Option<Retained<NSCursor>>) {
        *self.ivars().remote_cursor.borrow_mut() = cursor;
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    /// Repaint from the current framebuffer contents. Prefers the zero-copy
    /// IOSurface path; falls back to a pooled CGImage.
    pub fn refresh(&self) {
        let fb = self.ivars().framebuffer.borrow().clone();
        if let Some(fb) = fb {
            if let Some(layer) = self.layer() {
                if !ui::upload_framebuffer_iosurface(&layer, &fb, &self.ivars().surface_pool) {
                    ui::upload_framebuffer(&layer, &fb, &self.ivars().present_pool);
                }
            }
        }
    }

    fn send(&self, events: Vec<InputEvent>) {
        if events.is_empty() {
            return;
        }
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::Input(events));
        }
    }

    fn send_with_deferred_modifiers(&self, mut events: Vec<InputEvent>) {
        let mut routed = self
            .ivars()
            .input_routing
            .borrow_mut()
            .flush_deferred_command_modifiers();
        routed.append(&mut events);
        self.send(routed);
    }

    /// Release every held key on the remote when we lose focus, so modifiers
    /// (Hyper key, ⌘Tab, Mission Control) never get stuck on the host.
    pub fn release_all_keys(&self) {
        self.ivars().input_routing.borrow_mut().clear();
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::ReleaseAllKeys);
        }
    }

    fn mouse_button(&self, event: &NSEvent, button: PointerButton, down: bool) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send_with_deferred_modifiers(vec![InputEvent::MouseButton { button, down, x, y }]);
        }
    }

    /// Moves and scrolls leave a held ⌘ deferred: sending it would make a
    /// following ⌘C release a lone Windows key and open Start.
    fn mouse_move(&self, event: &NSEvent) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send(vec![InputEvent::MouseMove { x, y }]);
        }
    }

    /// Map an event's window-space location to remote framebuffer pixels.
    fn remote_point(&self, event: &NSEvent) -> Option<(u16, u16)> {
        let (fb_w, fb_h) = self.ivars().framebuffer.borrow().as_ref()?.dimensions();
        if fb_w == 0 || fb_h == 0 {
            return None;
        }
        let loc: NSPoint = event.locationInWindow();
        let local = self.convertPoint_fromView(loc, None);
        let bounds = self.bounds();
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return None;
        }
        let nx = (local.x / bounds.size.width).clamp(0.0, 1.0);
        // Flip Y: AppKit origin is bottom-left, RDP is top-left.
        let ny = 1.0 - (local.y / bounds.size.height).clamp(0.0, 1.0);
        let x = ((nx * f64::from(fb_w)) as u16).min(fb_w - 1);
        let y = ((ny * f64::from(fb_h)) as u16).min(fb_h - 1);
        Some((x, y))
    }
}

const DEVICE_MODIFIER_MASKS: usize = 0x0000_20ff;

#[derive(Default)]
struct InputRoutingState {
    pressed_modifiers: HashSet<u16>,
    deferred_command_modifiers: HashSet<u16>,
    suppressed_command_modifiers: HashSet<u16>,
    suppressed_key_ups: HashSet<u16>,
    external_paste_active: bool,
    /// Send ⌘C/X/V/A/Z/F/W as the Ctrl shortcut instead of the Windows key.
    mac_shortcuts: bool,
    /// ⌘ keys currently represented on the remote by one Ctrl key.
    ctrl_command_modifiers: HashSet<u16>,
}

impl InputRoutingState {
    fn is_pressed(&self, keycode: u16) -> bool {
        self.pressed_modifiers.contains(&keycode)
    }

    fn modifier_changed(
        &mut self,
        keycode: u16,
        down: bool,
        external_stt_paste_enabled: bool,
    ) -> Vec<InputEvent> {
        let changed = if down {
            self.pressed_modifiers.insert(keycode)
        } else {
            self.pressed_modifiers.remove(&keycode)
        };
        if !changed {
            return Vec::new();
        }
        if is_command_key(keycode) {
            if down && (external_stt_paste_enabled || self.mac_shortcuts) {
                self.deferred_command_modifiers.insert(keycode);
                return Vec::new();
            }
            if !down {
                if self.suppressed_command_modifiers.remove(&keycode) {
                    if self.suppressed_command_modifiers.is_empty() {
                        self.external_paste_active = false;
                    }
                    return Vec::new();
                }
                if self.ctrl_command_modifiers.remove(&keycode) {
                    return if self.ctrl_command_modifiers.is_empty() {
                        vec![InputEvent::Key {
                            keycode: LEFT_CONTROL_KEYCODE,
                            down: false,
                        }]
                    } else {
                        Vec::new()
                    };
                }
                if self.deferred_command_modifiers.remove(&keycode) {
                    return vec![
                        InputEvent::Key {
                            keycode,
                            down: true,
                        },
                        InputEvent::Key {
                            keycode,
                            down: false,
                        },
                    ];
                }
            }
        }
        vec![InputEvent::Key { keycode, down }]
    }

    /// `character` is the key's layout character ignoring modifiers other
    /// than Shift, when known; it decides which keys are Mac shortcuts.
    fn key_down(
        &mut self,
        keycode: u16,
        character: Option<char>,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        if is_command_key(keycode) {
            return (
                self.modifier_changed(keycode, true, external_stt_paste_enabled),
                false,
            );
        }
        if external_stt_paste_enabled
            && keycode == V_KEYCODE
            && self
                .pressed_modifiers
                .iter()
                .any(|key| is_command_key(*key))
        {
            return self.paste_invoked(true);
        }
        let mut events = if self.translates_to_ctrl(keycode, character) {
            self.command_as_ctrl()
        } else {
            let mut events = self.command_as_windows_key();
            events.extend(self.flush_deferred_command_modifiers());
            events
        };
        events.push(InputEvent::Key {
            keycode,
            down: true,
        });
        (events, false)
    }

    /// ⌘ (alone or with ⇧) plus a Mac editing shortcut key.
    fn translates_to_ctrl(&self, keycode: u16, character: Option<char>) -> bool {
        self.mac_shortcuts
            && self.active_command_keys().next().is_some()
            && !self.pressed_modifiers.iter().any(|key| {
                matches!(
                    *key,
                    LEFT_CONTROL_KEYCODE
                        | RIGHT_CONTROL_KEYCODE
                        | LEFT_OPTION_KEYCODE
                        | RIGHT_OPTION_KEYCODE
                )
            })
            && is_mac_shortcut(keycode, character)
    }

    /// Held ⌘ keys that are not swallowed by an external STT paste.
    fn active_command_keys(&self) -> impl Iterator<Item = u16> + '_ {
        self.pressed_modifiers
            .iter()
            .copied()
            .filter(|key| is_command_key(*key) && !self.suppressed_command_modifiers.contains(key))
    }

    /// Represent every held ⌘ as Ctrl on the remote.
    fn command_as_ctrl(&mut self) -> Vec<InputEvent> {
        let mut keycodes: Vec<_> = self
            .active_command_keys()
            .filter(|key| !self.ctrl_command_modifiers.contains(key))
            .collect();
        keycodes.sort_unstable();
        let mut events = Vec::new();
        for &keycode in &keycodes {
            // Not deferred means the Windows key is already down remotely.
            if !self.deferred_command_modifiers.remove(&keycode) {
                events.push(InputEvent::Key {
                    keycode,
                    down: false,
                });
            }
        }
        if self.ctrl_command_modifiers.is_empty() && !keycodes.is_empty() {
            events.push(InputEvent::Key {
                keycode: LEFT_CONTROL_KEYCODE,
                down: true,
            });
        }
        self.ctrl_command_modifiers.extend(keycodes);
        events
    }

    /// Undo `command_as_ctrl`: release Ctrl and press the held ⌘ keys again.
    fn command_as_windows_key(&mut self) -> Vec<InputEvent> {
        if self.ctrl_command_modifiers.is_empty() {
            return Vec::new();
        }
        let mut keycodes: Vec<_> = self.ctrl_command_modifiers.drain().collect();
        keycodes.sort_unstable();
        let mut events = vec![InputEvent::Key {
            keycode: LEFT_CONTROL_KEYCODE,
            down: false,
        }];
        events.extend(keycodes.into_iter().map(|keycode| InputEvent::Key {
            keycode,
            down: true,
        }));
        events
    }

    fn key_up(&mut self, keycode: u16, external_stt_paste_enabled: bool) -> Vec<InputEvent> {
        if is_command_key(keycode) {
            return self.modifier_changed(keycode, false, external_stt_paste_enabled);
        }
        if self.suppressed_key_ups.remove(&keycode) {
            if keycode == V_KEYCODE {
                self.external_paste_active = false;
            }
            return Vec::new();
        }
        vec![InputEvent::Key {
            keycode,
            down: false,
        }]
    }

    fn key_pulse(
        &mut self,
        keycode: u16,
        character: Option<char>,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        let (mut events, external_paste) =
            self.key_down(keycode, character, external_stt_paste_enabled);
        events.extend(self.key_up(keycode, external_stt_paste_enabled));
        (events, external_paste)
    }

    fn paste_invoked(&mut self, external_stt_paste_enabled: bool) -> (Vec<InputEvent>, bool) {
        if !external_stt_paste_enabled {
            return (Vec::new(), false);
        }
        let command_keys: Vec<_> = self
            .pressed_modifiers
            .iter()
            .copied()
            .filter(|key| is_command_key(*key))
            .collect();
        if !command_keys.is_empty() {
            self.suppressed_key_ups.insert(V_KEYCODE);
        }
        let submit = !self.external_paste_active;
        if !command_keys.is_empty() {
            self.external_paste_active = true;
        }

        let mut events = Vec::new();
        for keycode in command_keys {
            if self.deferred_command_modifiers.remove(&keycode) {
                self.suppressed_command_modifiers.insert(keycode);
            } else if self.ctrl_command_modifiers.remove(&keycode) {
                self.suppressed_command_modifiers.insert(keycode);
                if self.ctrl_command_modifiers.is_empty() {
                    events.push(InputEvent::Key {
                        keycode: LEFT_CONTROL_KEYCODE,
                        down: false,
                    });
                }
            } else if !self.suppressed_command_modifiers.contains(&keycode) {
                // The setting may have been enabled while Command was already
                // held. Release that forwarded modifier immediately.
                self.suppressed_command_modifiers.insert(keycode);
                events.push(InputEvent::Key {
                    keycode,
                    down: false,
                });
            }
        }
        (events, submit)
    }

    fn flush_deferred_command_modifiers(&mut self) -> Vec<InputEvent> {
        let mut keycodes: Vec<_> = self.deferred_command_modifiers.drain().collect();
        keycodes.sort_unstable();
        keycodes
            .into_iter()
            .map(|keycode| InputEvent::Key {
                keycode,
                down: true,
            })
            .collect()
    }

    fn clear(&mut self) {
        self.pressed_modifiers.clear();
        self.deferred_command_modifiers.clear();
        self.suppressed_command_modifiers.clear();
        self.suppressed_key_ups.clear();
        self.external_paste_active = false;
        self.ctrl_command_modifiers.clear();
    }
}

const LEFT_COMMAND_KEYCODE: u16 = 0x37;
const RIGHT_COMMAND_KEYCODE: u16 = 0x36;
const LEFT_CONTROL_KEYCODE: u16 = 0x3b;
const RIGHT_CONTROL_KEYCODE: u16 = 0x3e;
const LEFT_OPTION_KEYCODE: u16 = 0x3a;
const RIGHT_OPTION_KEYCODE: u16 = 0x3d;
const A_KEYCODE: u16 = 0x00;
const C_KEYCODE: u16 = 0x08;
const F_KEYCODE: u16 = 0x03;
const V_KEYCODE: u16 = 0x09;
const W_KEYCODE: u16 = 0x0d;
const X_KEYCODE: u16 = 0x07;
const Z_KEYCODE: u16 = 0x06;

fn is_command_key(keycode: u16) -> bool {
    matches!(keycode, LEFT_COMMAND_KEYCODE | RIGHT_COMMAND_KEYCODE)
}

/// The shortcuts Windows App translates: copy, cut, paste, select all, undo,
/// find, close. Non-Latin layouts fall back to the US key position, as macOS
/// does for ⌘ shortcuts.
fn is_mac_shortcut(keycode: u16, character: Option<char>) -> bool {
    match character {
        Some(c) if c.is_ascii() => {
            matches!(
                c.to_ascii_lowercase(),
                'a' | 'c' | 'f' | 'v' | 'w' | 'x' | 'z'
            )
        }
        _ => matches!(
            keycode,
            A_KEYCODE | C_KEYCODE | F_KEYCODE | V_KEYCODE | W_KEYCODE | X_KEYCODE | Z_KEYCODE
        ),
    }
}

fn modifier_masks(keycode: u16) -> Option<(usize, usize)> {
    Some(match keycode {
        0x3b => (0x0000_0001, 0x0004_0000), // left control
        0x38 => (0x0000_0002, 0x0002_0000), // left shift
        0x3c => (0x0000_0004, 0x0002_0000), // right shift
        0x37 => (0x0000_0008, 0x0010_0000), // left command
        0x36 => (0x0000_0010, 0x0010_0000), // right command
        0x3a => (0x0000_0020, 0x0008_0000), // left option
        0x3d => (0x0000_0040, 0x0008_0000), // right option
        0x39 => (0x0000_0080, 0x0001_0000), // caps lock
        0x3e => (0x0000_2000, 0x0004_0000), // right control
        _ => return None,
    })
}

/// The key's layout character, ignoring modifiers other than Shift.
pub fn key_character(event: &NSEvent) -> Option<char> {
    event
        .charactersIgnoringModifiers()?
        .to_string()
        .chars()
        .next()
}

fn external_stt_paste_text(enabled: bool, text: Option<&str>) -> Option<String> {
    enabled
        .then_some(text)
        .flatten()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{external_stt_paste_text, modifier_masks, InputRoutingState};
    use rdp123_core::InputEvent;

    #[test]
    fn modifier_keycodes_use_distinct_device_masks() {
        assert_ne!(
            modifier_masks(0x38).unwrap().0,
            modifier_masks(0x3c).unwrap().0
        );
        assert_ne!(
            modifier_masks(0x37).unwrap().0,
            modifier_masks(0x36).unwrap().0
        );
        assert_eq!(modifier_masks(0x00), None);
    }

    #[test]
    fn external_stt_paste_requires_the_global_setting_and_text() {
        assert_eq!(external_stt_paste_text(false, Some("dictated text")), None);
        assert_eq!(external_stt_paste_text(true, None), None);
        assert_eq!(external_stt_paste_text(true, Some("")), None);
        assert_eq!(
            external_stt_paste_text(true, Some("dictated text")),
            Some("dictated text".to_string())
        );
    }

    #[test]
    fn external_stt_paste_shortcut_never_reaches_windows() {
        let mut routing = InputRoutingState::default();
        let mut remote_events = routing.modifier_changed(0x37, true, true);
        let (paste_events, submit) = routing.paste_invoked(true);
        remote_events.extend(paste_events);
        remote_events.extend(routing.key_up(0x09, true));
        remote_events.extend(routing.modifier_changed(0x37, false, true));

        assert!(submit);
        assert!(
            remote_events.is_empty(),
            "the macOS Command+V shortcut leaked into the remote session"
        );
    }

    #[test]
    fn stt_runtime_regression_synthetic_command_key_events_never_reach_windows() {
        let mut routing = InputRoutingState::default();
        let (mut remote_events, _) = routing.key_down(0x37, None, true);
        let (paste_events, submit) = routing.paste_invoked(true);
        remote_events.extend(paste_events);
        remote_events.extend(routing.key_up(0x09, true));
        remote_events.extend(routing.key_up(0x37, true));

        assert!(submit);
        assert!(
            remote_events.is_empty(),
            "synthetic Command down/up leaked into the remote session"
        );
    }

    #[test]
    fn ordinary_command_shortcuts_still_reach_windows() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        let (events, external_paste) = routing.key_down(0x08, None, true);

        assert!(!external_paste);
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x08,
                    down: true
                }
            ]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn appkit_command_shortcuts_include_the_missing_key_release() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        let (events, external_paste) = routing.key_pulse(0x0f, None, true);

        assert!(!external_paste);
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x0f,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x0f,
                    down: false
                }
            ]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn command_input_is_unchanged_when_external_stt_paste_is_disabled() {
        let mut routing = InputRoutingState::default();

        assert!(matches!(
            routing.modifier_changed(0x37, true, false).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: true
            }]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, false).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn command_tap_is_preserved_when_external_stt_paste_is_enabled() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x37,
                    down: false
                }
            ]
        ));
    }

    fn key(keycode: u16, down: bool) -> InputEvent {
        InputEvent::Key { keycode, down }
    }

    fn mac_shortcut_routing() -> InputRoutingState {
        InputRoutingState {
            mac_shortcuts: true,
            ..Default::default()
        }
    }

    #[test]
    fn mac_shortcut_sends_ctrl_instead_of_the_windows_key() {
        let mut routing = mac_shortcut_routing();
        assert!(routing.modifier_changed(0x37, true, false).is_empty());

        let (events, _) = routing.key_pulse(0x08, Some('c'), false);
        assert_eq!(events, [key(0x3b, true), key(0x08, true), key(0x08, false)]);
        assert_eq!(
            routing.modifier_changed(0x37, false, false),
            [key(0x3b, false)]
        );
    }

    #[test]
    fn mac_shortcut_follows_the_layout_character_not_the_key_position() {
        // Swiss German QWERTZ: Z sits on the US Y key and Y on the US Z key.
        let mut routing = mac_shortcut_routing();
        routing.modifier_changed(0x37, true, false);
        let (undo, _) = routing.key_pulse(0x10, Some('z'), false);
        assert_eq!(undo, [key(0x3b, true), key(0x10, true), key(0x10, false)]);
        routing.modifier_changed(0x37, false, false);

        routing.modifier_changed(0x37, true, false);
        let (win_y, _) = routing.key_pulse(0x06, Some('y'), false);
        assert_eq!(win_y, [key(0x37, true), key(0x06, true), key(0x06, false)]);
        assert_eq!(
            routing.modifier_changed(0x37, false, false),
            [key(0x37, false)]
        );
    }

    #[test]
    fn mac_shortcut_falls_back_to_key_position_for_non_latin_layouts() {
        let mut routing = mac_shortcut_routing();
        routing.modifier_changed(0x37, true, false);
        let (events, _) = routing.key_pulse(0x08, Some('с'), false); // Cyrillic es
        assert_eq!(events, [key(0x3b, true), key(0x08, true), key(0x08, false)]);
    }

    #[test]
    fn mac_shortcut_keeps_command_tap_as_windows_key() {
        let mut routing = mac_shortcut_routing();
        assert!(routing.modifier_changed(0x37, true, false).is_empty());
        assert_eq!(
            routing.modifier_changed(0x37, false, false),
            [key(0x37, true), key(0x37, false)]
        );
    }

    #[test]
    fn mac_shortcut_switches_back_to_windows_key_for_other_keys() {
        let mut routing = mac_shortcut_routing();
        routing.modifier_changed(0x37, true, false);
        routing.key_pulse(0x08, Some('c'), false);

        let (events, _) = routing.key_pulse(0x0e, Some('e'), false);
        assert_eq!(
            events,
            [
                key(0x3b, false),
                key(0x37, true),
                key(0x0e, true),
                key(0x0e, false)
            ]
        );
        let (events, _) = routing.key_pulse(0x09, Some('v'), false);
        assert_eq!(
            events,
            [
                key(0x37, false),
                key(0x3b, true),
                key(0x09, true),
                key(0x09, false)
            ]
        );
        assert_eq!(
            routing.modifier_changed(0x37, false, false),
            [key(0x3b, false)]
        );
    }

    #[test]
    fn mac_shortcut_allows_shift() {
        let mut routing = mac_shortcut_routing();
        routing.modifier_changed(0x38, true, false);
        routing.modifier_changed(0x37, true, false);
        let (events, _) = routing.key_pulse(0x06, Some('Z'), false);
        assert_eq!(events, [key(0x3b, true), key(0x06, true), key(0x06, false)]);
    }

    #[test]
    fn mac_shortcut_is_not_applied_with_option_or_control() {
        for other in [0x3a, 0x3b] {
            let mut routing = mac_shortcut_routing();
            routing.modifier_changed(other, true, false);
            routing.modifier_changed(0x37, true, false);
            let (events, _) = routing.key_pulse(0x08, Some('c'), false);
            assert_eq!(events, [key(0x37, true), key(0x08, true), key(0x08, false)]);
        }
    }

    #[test]
    fn external_stt_paste_releases_a_translated_ctrl() {
        let mut routing = mac_shortcut_routing();
        routing.modifier_changed(0x37, true, true);
        routing.key_pulse(0x08, Some('c'), true);

        let (events, submit) = routing.key_down(0x09, Some('v'), true);
        assert!(submit);
        assert_eq!(events, [key(0x3b, false)]);
        assert!(routing.key_up(0x09, true).is_empty());
        assert!(routing.modifier_changed(0x37, false, true).is_empty());
    }

    #[test]
    fn duplicate_key_and_menu_paste_dispatch_submits_only_once() {
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);

        let (_, key_dispatch_submits) = routing.key_down(0x09, None, true);
        let (_, menu_dispatch_submits) = routing.paste_invoked(true);

        assert!(key_dispatch_submits);
        assert!(!menu_dispatch_submits);
    }

    #[test]
    fn command_release_finishes_paste_when_appkit_consumes_v_key_up() {
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);
        let (_, first_submits) = routing.paste_invoked(true);
        routing.modifier_changed(0x37, false, true);

        routing.modifier_changed(0x37, true, true);
        let (_, second_submits) = routing.paste_invoked(true);

        assert!(first_submits);
        assert!(second_submits);
    }
}
