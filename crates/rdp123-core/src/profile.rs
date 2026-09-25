//! Connection profiles and their on-disk store.
//!
//! Profiles live in a single JSON file under the app's config directory.
//! Passwords are NEVER stored here — only in the macOS Keychain (see
//! [`crate::secrets`]). The file is safe to hand-edit.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const QUALIFIER: &str = "ch";
const ORG: &str = "asd123";
/// Organization used by builds before the bundle-identifier change; an
/// existing connections file there is moved over on first start.
const LEGACY_ORG: &str = "rdp123";
const APP: &str = "RDP123";
const DOCUMENT_VERSION: u32 = 1;
const MAX_RECONNECTS_PER_MINUTE: u32 = 60;
pub(crate) const MIN_REMOTE_DIMENSION: u16 = 200;
pub(crate) const MAX_REMOTE_DIMENSION: u16 = 8192;

/// The kind of remote a profile connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionKind {
    Rdp,
    Ssh,
}

/// A single saved connection.
/// Colour depth of the remote session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ColorQuality {
    #[default]
    High32,
    Medium16,
}

impl ColorQuality {
    pub fn bits(self) -> u32 {
        match self {
            Self::High32 => 32,
            Self::Medium16 => 16,
        }
    }
}

/// Clipboard sharing direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardMode {
    #[default]
    Bidirectional,
    Disabled,
    LocalToRemote,
    RemoteToLocal,
}

impl ClipboardMode {
    pub fn enabled(self) -> bool {
        self != Self::Disabled
    }
    pub fn allow_remote_to_local(self) -> bool {
        matches!(self, Self::Bidirectional | Self::RemoteToLocal)
    }
    pub fn allow_local_to_remote(self) -> bool {
        matches!(self, Self::Bidirectional | Self::LocalToRemote)
    }
}

/// Where remote audio is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AudioMode {
    /// Redirect to this Mac (RDPSND channel + local playback).
    #[default]
    ThisComputer,
    /// Tell the server to discard audio entirely.
    Never,
    /// Best effort: don't redirect and don't suppress. True console audio
    /// (INFO_REMOTECONSOLEAUDIO) is not exposed by IronRDP.
    RemoteComputer,
}

/// Which graphics pipeline to request from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GraphicsMode {
    /// RDP 8 Graphics Pipeline (EGFX): H.264/AVC420 and RemoteFX Progressive.
    /// Still experimental: rare compositing artifacts remain.
    Egfx,
    /// Legacy bitmap updates. Default until EGFX is artifact-free.
    #[default]
    Classic,
}

/// Desktop scaling (DPI) level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScalingLevel {
    #[default]
    Auto,
    Percent100,
    Percent140,
    Percent180,
    Percent200,
}

impl ScalingLevel {
    /// Explicit percentage, or `None` to follow the display (Retina => 200).
    pub fn percent(self) -> Option<u32> {
        match self {
            Self::Auto => None,
            Self::Percent100 => Some(100),
            Self::Percent140 => Some(140),
            Self::Percent180 => Some(180),
            Self::Percent200 => Some(200),
        }
    }
}

/// How the remote resolution is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ResolutionMode {
    #[default]
    FitToWindow,
    Fixed,
}

/// Password handling for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PasswordPolicy {
    #[default]
    Remember,
    AlwaysAsk,
}

/// Authentication protocol used for an RDP connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticationMode {
    /// Network Level Authentication using the saved or prompted password.
    #[default]
    Password,
    /// Microsoft Entra web authentication (`enablerdsaadauth:i:1`).
    EntraWeb,
}

fn default_true() -> bool {
    true
}
fn default_reconnect_rate() -> u32 {
    6
}

/// RDP-specific options (ignored for SSH connections).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RdpOptions {
    /// Password/CredSSP or Microsoft Entra web authentication.
    #[serde(default)]
    pub authentication: AuthenticationMode,
    /// Password authentication only: also offer TLS, so servers without NLA
    /// (xrdp) can accept the connection. Standard RDP security stays refused.
    #[serde(default)]
    pub allow_tls_without_nla: bool,
    #[serde(default)]
    pub color_quality: ColorQuality,
    /// FastPath transport compression (RDP 6.1 XCRUSH), independent of the
    /// selected graphics codec.
    #[serde(default = "default_true")]
    pub compression: bool,
    #[serde(default)]
    pub clipboard: ClipboardMode,
    /// Where remote audio plays.
    #[serde(default)]
    pub audio: AudioMode,
    /// Graphics pipeline (EGFX or legacy bitmap updates).
    #[serde(default)]
    pub graphics: GraphicsMode,
    /// EGFX only: offer AVC444 (the V10.7 capability set). Off falls back to
    /// AVC420 for servers whose AVC444 stream is broken.
    #[serde(default = "default_true")]
    pub avc444: bool,
    #[serde(default)]
    pub scaling: ScalingLevel,
    #[serde(default)]
    pub resolution_mode: ResolutionMode,
    /// For `ResolutionMode::Fixed`: (width, height) in logical pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<(u16, u16)>,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub reconnect: bool,
    #[serde(default = "default_reconnect_rate")]
    pub reconnect_per_minute: u32,
    #[serde(default)]
    pub password_policy: PasswordPolicy,
    /// Restore the last window size on the next connect (fit-to-window only).
    #[serde(default = "default_true")]
    pub remember_size: bool,
    /// Wake-on-LAN: MAC address of the host. When set, a magic packet is
    /// broadcast before connecting and the initial retry window is extended
    /// so the machine has time to wake up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mac: Option<String>,
    /// Keep the remote session awake: while idle, tap an invisible F15 key so
    /// idle-disconnect policies and the remote lock screen never trigger. Off
    /// by default because it also prevents the host from auto-locking.
    #[serde(default)]
    pub keep_alive: bool,
    /// Last window content size in points, saved when a session closes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_window_size: Option<(u16, u16)>,
}

impl Default for RdpOptions {
    fn default() -> Self {
        Self {
            authentication: AuthenticationMode::default(),
            allow_tls_without_nla: false,
            color_quality: ColorQuality::default(),
            compression: true,
            clipboard: ClipboardMode::default(),
            audio: AudioMode::default(),
            graphics: GraphicsMode::default(),
            avc444: true,
            scaling: ScalingLevel::default(),
            resolution_mode: ResolutionMode::default(),
            resolution: None,
            fullscreen: false,
            reconnect: false,
            reconnect_per_minute: default_reconnect_rate(),
            password_policy: PasswordPolicy::default(),
            remember_size: true,
            wake_mac: None,
            keep_alive: false,
            last_window_size: None,
        }
    }
}

/// A single saved connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    /// Stable identifier, also used as the Keychain account name.
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// RDP only: pinned SHA-256 of the server's public key (trust-on-first-use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_fingerprint: Option<String>,
    /// RDP-specific options.
    #[serde(default)]
    pub rdp: RdpOptions,
}

impl Connection {
    pub fn default_port(kind: ConnectionKind) -> u16 {
        match kind {
            ConnectionKind::Rdp => 3389,
            ConnectionKind::Ssh => 22,
        }
    }

    /// A fresh connection with a generated id and sensible defaults.
    pub fn new(name: impl Into<String>, kind: ConnectionKind) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            kind,
            host: String::new(),
            port: Self::default_port(kind),
            username: String::new(),
            domain: None,
            cert_fingerprint: None,
            rdp: RdpOptions::default(),
        }
    }

    /// A copy under a new id and name. The pinned fingerprint belongs to
    /// this connection's host and is not carried over; the copy is usually
    /// pointed somewhere else, so it pins on its first connect.
    pub fn duplicate(&self, name: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            cert_fingerprint: None,
            ..self.clone()
        }
    }

    /// Point the connection at `host`. The pinned fingerprint belongs to the
    /// old host, so a different host drops it and pins on its next connect.
    pub fn set_host(&mut self, host: impl Into<String>) {
        let host = host.into();
        if !host.eq_ignore_ascii_case(&self.host) {
            self.cert_fingerprint = None;
        }
        self.host = host;
    }

    pub fn validate(&self) -> Result<()> {
        validate_label("connection id", &self.id)?;
        validate_label("connection name", &self.name)?;
        validate_host(&self.host)?;
        if self.port == 0 {
            bail!("port must be between 1 and 65535");
        }
        if self.username.chars().any(char::is_control) {
            bail!("username must not contain control characters");
        }
        if self
            .domain
            .as_deref()
            .is_some_and(|domain| domain.chars().any(char::is_control))
        {
            bail!("domain must not contain control characters");
        }
        if self.kind == ConnectionKind::Rdp
            && self.rdp.authentication == AuthenticationMode::EntraWeb
            && is_ip_address(&self.host)
        {
            bail!("Microsoft Entra web authentication requires a hostname, not an IP address");
        }
        if !(1..=MAX_RECONNECTS_PER_MINUTE).contains(&self.rdp.reconnect_per_minute) {
            bail!(
                "maximum reconnect attempts must be between 1 and {MAX_RECONNECTS_PER_MINUTE} per minute"
            );
        }
        if self.rdp.resolution_mode == ResolutionMode::Fixed {
            let (width, height) = self
                .rdp
                .resolution
                .context("fixed resolution requires a width and height")?;
            if !(MIN_REMOTE_DIMENSION..=MAX_REMOTE_DIMENSION).contains(&width)
                || !(MIN_REMOTE_DIMENSION..=MAX_REMOTE_DIMENSION).contains(&height)
            {
                bail!(
                    "fixed resolution must be between {MIN_REMOTE_DIMENSION} and \
                     {MAX_REMOTE_DIMENSION} pixels per dimension"
                );
            }
        }
        Ok(())
    }
}

fn is_ip_address(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .is_some_and(|host| host.parse::<IpAddr>().is_ok())
}

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn validate_label(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    if value.chars().any(char::is_control) {
        bail!("{label} must not contain control characters");
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<()> {
    validate_label("host", host)?;
    if host.starts_with('-') {
        bail!("host must not start with '-'");
    }
    if host.chars().any(char::is_whitespace) {
        bail!("host must not contain whitespace");
    }
    Ok(())
}

/// The terminal used to open SSH sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalKind {
    #[default]
    Kaku,
    Wezterm,
    Ghostty,
    Alacritty,
    Iterm,
    Terminal,
    Custom,
}

impl TerminalKind {
    /// All variants, in menu order.
    pub const ALL: [TerminalKind; 7] = [
        Self::Kaku,
        Self::Wezterm,
        Self::Ghostty,
        Self::Alacritty,
        Self::Iterm,
        Self::Terminal,
        Self::Custom,
    ];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Kaku => "Kaku",
            Self::Wezterm => "WezTerm",
            Self::Ghostty => "Ghostty",
            Self::Alacritty => "Alacritty",
            Self::Iterm => "iTerm2",
            Self::Terminal => "Terminal",
            Self::Custom => "Custom command…",
        }
    }
}

/// Global settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub terminal: TerminalKind,
    /// Swap ⌘ and ⌥ inside RDP sessions: ⌘ sends Alt (PC key position next
    /// to the space bar) and ⌥ sends the Windows key. Off by default.
    #[serde(default)]
    pub swap_cmd_alt: bool,
    /// Send ⌘C, ⌘V, ⌘X, ⌘A, ⌘Z, ⌘F and ⌘W as their Ctrl shortcuts inside RDP
    /// sessions. Other ⌘ combinations keep the Windows key. On by default.
    #[serde(default = "default_true")]
    pub mac_shortcuts: bool,
    /// Support external speech-to-text tools that paste through macOS. When
    /// enabled, RDP123 synchronizes the clipboard before sending remote Ctrl+V.
    #[serde(default)]
    pub external_stt_paste: bool,
    /// For `TerminalKind::Custom`: a shell command line where `{ssh}` is
    /// replaced by the `ssh …` invocation (also `{host}`, `{port}`, `{user}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_terminal: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            terminal: TerminalKind::default(),
            swap_cmd_alt: false,
            mac_shortcuts: true,
            external_stt_paste: false,
            custom_terminal: None,
        }
    }
}

/// The top-level document persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub version: u32,
    #[serde(default)]
    pub connections: Vec<Connection>,
    #[serde(default)]
    pub settings: Settings,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: DOCUMENT_VERSION,
            connections: Vec::new(),
            settings: Settings::default(),
        }
    }
}

impl Document {
    pub fn validate(&self) -> Result<()> {
        if self.version != DOCUMENT_VERSION {
            bail!(
                "unsupported connections file version {} (expected {DOCUMENT_VERSION})",
                self.version
            );
        }
        let mut ids = HashSet::with_capacity(self.connections.len());
        for connection in &self.connections {
            connection
                .validate()
                .with_context(|| format!("invalid connection {:?}", connection.name))?;
            if !ids.insert(connection.id.as_str()) {
                bail!("duplicate connection id {:?}", connection.id);
            }
        }
        Ok(())
    }
}

/// Reads and writes the persisted document.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    /// Open the store at the default per-user config location.
    pub fn open_default() -> Result<Self> {
        let dirs = ProjectDirs::from(QUALIFIER, ORG, APP)
            .context("could not resolve a config directory")?;
        let dir = dirs.config_dir().to_path_buf();
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
        #[cfg(unix)]
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing config dir {}", dir.display()))?;
        let path = dir.join("connections.json");

        // One-time migration from the pre-bundle-id-change location.
        if !path.exists() {
            if let Some(legacy) = ProjectDirs::from(QUALIFIER, LEGACY_ORG, APP) {
                let legacy_path = legacy.config_dir().join("connections.json");
                if legacy_path.exists() {
                    fs::rename(&legacy_path, &path).with_context(|| {
                        format!("migrating {} to {}", legacy_path.display(), path.display())
                    })?;
                }
            }
        }

        Ok(Self { path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Load the whole document. A missing file yields defaults.
    pub fn load_document(&self) -> Result<Document> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let document: Document = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", self.path.display()))?;
                document
                    .validate()
                    .with_context(|| format!("validating {}", self.path.display()))?;
                Ok(document)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Document::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Persist the whole document (pretty-printed, atomic replace).
    pub fn save_document(&self, document: &Document) -> Result<()> {
        document.validate()?;
        let json = serde_json::to_vec_pretty(document)?;
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            file.write_all(&json)
                .with_context(|| format!("writing {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing {}", tmp.display()))?;
            fs::rename(&tmp, &self.path)
                .with_context(|| format!("replacing {}", self.path.display()))?;
            if let Some(parent) = self.path.parent() {
                File::open(parent)
                    .and_then(|dir| dir.sync_all())
                    .with_context(|| format!("syncing {}", parent.display()))?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        Ok(())
    }

    /// Load just the connection list.
    pub fn load(&self) -> Result<Vec<Connection>> {
        Ok(self.load_document()?.connections)
    }

    /// Replace the connection list, preserving other settings.
    pub fn save(&self, connections: &[Connection]) -> Result<()> {
        let mut doc = self.load_document()?;
        doc.connections = connections.to_vec();
        self.save_document(&doc)
    }

    /// Update just the pinned fingerprint for one connection and persist.
    pub fn set_fingerprint(&self, id: &str, fingerprint: &str) -> Result<()> {
        let mut doc = self.load_document()?;
        let connection = doc
            .connections
            .iter_mut()
            .find(|connection| connection.id == id)
            .with_context(|| format!("connection id {id:?} no longer exists"))?;
        connection.cert_fingerprint = Some(fingerprint.to_string());
        self.save_document(&doc)?;
        Ok(())
    }

    /// Save the last window size for one connection (best-effort, ignores a
    /// missing connection). Called when a session window closes.
    pub fn set_last_window_size(&self, id: &str, size: (u16, u16)) -> Result<()> {
        let mut doc = self.load_document()?;
        if let Some(c) = doc.connections.iter_mut().find(|c| c.id == id) {
            c.rdp.last_window_size = Some(size);
            self.save_document(&doc)?;
        }
        Ok(())
    }

    /// Update the password policy for one connection and persist.
    pub fn set_password_policy(&self, id: &str, policy: PasswordPolicy) -> Result<()> {
        let mut doc = self.load_document()?;
        if let Some(c) = doc.connections.iter_mut().find(|c| c.id == id) {
            c.rdp.password_policy = policy;
            self.save_document(&doc)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (ProfileStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rdp123-profile-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        (
            ProfileStore {
                path: dir.join("connections.json"),
            },
            dir,
        )
    }

    #[test]
    fn changing_the_host_drops_the_pinned_fingerprint() {
        let mut connection = Connection::new("Office", ConnectionKind::Rdp);
        connection.host = "server.example".to_string();
        connection.cert_fingerprint = Some("ab:cd".to_string());

        connection.set_host("SERVER.example");
        assert_eq!(connection.cert_fingerprint.as_deref(), Some("ab:cd"));

        connection.set_host("other.example");
        assert_eq!(connection.host, "other.example");
        assert_eq!(connection.cert_fingerprint, None);
    }

    #[test]
    fn rejects_invalid_and_duplicate_connections() {
        let mut document = Document::default();
        let mut connection = Connection::new("Office", ConnectionKind::Rdp);
        connection.host = "server.example".to_string();
        document.connections.push(connection.clone());
        document.connections.push(connection);
        assert!(document
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        document.connections.truncate(1);
        document.connections[0].host = "-oProxyCommand=evil".to_string();
        assert!(format!("{:#}", document.validate().unwrap_err()).contains("must not start"));
    }

    #[test]
    fn a_duplicate_gets_a_new_id_and_no_pinned_fingerprint() {
        let mut original = Connection::new("Office", ConnectionKind::Rdp);
        original.host = "server.example".to_string();
        original.username = "willie".to_string();
        original.cert_fingerprint = Some("ab:cd".to_string());
        original.rdp.fullscreen = true;

        let copy = original.duplicate("Office copy");
        assert_ne!(copy.id, original.id);
        assert_eq!(copy.name, "Office copy");
        assert_eq!(copy.cert_fingerprint, None);
        assert_eq!(copy.host, original.host);
        assert_eq!(copy.username, original.username);
        assert!(copy.rdp.fullscreen);

        let document = Document {
            connections: vec![original, copy],
            ..Document::default()
        };
        document.validate().unwrap();
    }

    #[test]
    fn entra_web_auth_rejects_ip_addresses() {
        let mut connection = Connection::new("Entra PC", ConnectionKind::Rdp);
        connection.host = "192.0.2.10".to_string();
        connection.rdp.authentication = AuthenticationMode::EntraWeb;
        assert!(connection
            .validate()
            .unwrap_err()
            .to_string()
            .contains("hostname"));

        connection.host = "entra-pc.example.com".to_string();
        assert!(connection.validate().is_ok());
    }

    #[test]
    fn missing_authentication_mode_defaults_to_password() {
        let json = r#"{
            "version": 1,
            "connections": [{
                "id": "legacy-profile",
                "name": "Legacy",
                "kind": "rdp",
                "host": "legacy.example.com",
                "port": 3389,
                "rdp": {}
            }]
        }"#;
        let document: Document = serde_json::from_str(json).unwrap();
        assert_eq!(
            document.connections[0].rdp.authentication,
            AuthenticationMode::Password
        );
    }

    #[test]
    fn missing_external_stt_paste_setting_defaults_off() {
        let document: Document =
            serde_json::from_str(r#"{"version":1,"connections":[],"settings":{}}"#).unwrap();

        assert!(!document.settings.external_stt_paste);
    }

    #[test]
    fn missing_mac_shortcuts_setting_defaults_on() {
        let field_missing: Document =
            serde_json::from_str(r#"{"version":1,"connections":[],"settings":{}}"#).unwrap();
        let settings_missing: Document =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert!(field_missing.settings.mac_shortcuts);
        assert!(settings_missing.settings.mac_shortcuts);
    }

    #[test]
    fn external_stt_paste_setting_round_trips() {
        let mut document = Document::default();
        document.settings.external_stt_paste = true;

        let json = serde_json::to_string(&document).unwrap();
        let restored: Document = serde_json::from_str(&json).unwrap();

        assert!(restored.settings.external_stt_paste);
    }

    #[test]
    fn saves_and_loads_valid_document_atomically() {
        let (store, dir) = test_store();
        let mut document = Document::default();
        let mut connection = Connection::new("Office", ConnectionKind::Rdp);
        connection.host = "server.example".to_string();
        document.connections.push(connection);

        store.save_document(&document).unwrap();
        let loaded = store.load_document().unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].host, "server.example");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_save_keeps_the_last_valid_document() {
        let (store, dir) = test_store();
        let mut document = Document::default();
        let mut connection = Connection::new("Office", ConnectionKind::Rdp);
        connection.host = "server.example".to_string();
        document.connections.push(connection);
        store.save_document(&document).unwrap();

        document.connections[0].host.clear();
        assert!(store.save_document(&document).is_err());
        assert_eq!(
            store.load_document().unwrap().connections[0].host,
            "server.example"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_document_is_reported_instead_of_replaced_with_defaults() {
        let (store, dir) = test_store();
        fs::write(store.path(), b"{ definitely not json").unwrap();
        assert!(store.load_document().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unknown_document_version() {
        let document = Document {
            version: DOCUMENT_VERSION + 1,
            ..Document::default()
        };
        assert!(document
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
    }
}
