//! Core, UI-independent building blocks for the RDP123 client.
//!
//! Everything here is testable without AppKit: profile storage, Keychain
//! access, the RDP session engine, input mapping and the shared framebuffer.
//! The `rdp123-app` crate provides the macOS front-end on top.

pub mod audio;
pub mod framebuffer;
pub mod gfx;
pub mod keymap;
pub mod profile;
mod rdsaad;
pub mod secrets;
pub mod session;
pub mod terminal;
mod tls;
pub mod videotoolbox;
pub mod wol;

pub use framebuffer::SharedFramebuffer;
pub use profile::{
    AudioMode, AuthenticationMode, ClipboardMode, ColorQuality, Connection, ConnectionKind,
    Document, GraphicsMode, PasswordPolicy, ProfileStore, RdpOptions, ResolutionMode, ScalingLevel,
    Settings, TerminalKind,
};
pub use session::{
    spawn as spawn_session, InputEvent, PointerButton, SessionCommand, SessionConfig, SessionEvent,
    SessionHandle, REMOTE_ENDED,
};
