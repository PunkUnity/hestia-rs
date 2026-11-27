//! UI-agnostic client facade. The graphical client (`hestia-frontend`, drawn
//! by a UI shell) and the CLI both drive hosts through [`HestiaClient`]:
//!
//! * loads or creates the client identity,
//! * keeps the list of known hosts (manual + discovered) on disk,
//! * pairs, lists apps, fetches box art,
//! * probes Hermes' Hestia API and uses it when present,
//! * launches/quits apps, handing the streaming layer what it needs.
//!
//! All methods are blocking and thread-safe; call them from worker threads.

pub mod address;
pub mod diagnosis;
pub mod hostaddr;
pub mod logcapture;
pub mod presets;
pub mod profiles;
pub mod report;
pub mod settings;
pub mod shortcuts;
pub mod store;
pub mod wol;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hestia_api::{
    AppRequest, Capabilities, ClientDescription, HestiaApi, SessionPrepareRequest, SessionPrepareResponse,
    StreamRequest, VirtualDisplayRequest,
};
use hestia_crypto::{ClientIdentity, pem};
use hestia_discovery::{DiscoveredHost, netif};
use hestia_nvhttp::{
    AppInfo, AudioConfig, DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT, LaunchRequest, LaunchResponse, NvHttpClient,
    PairingOutcome, PairingPin, ServerInfo,
};
use log::{info, warn};
use serde::{Deserialize, Serialize};

pub use hestia_api as api;
pub use hestia_discovery as discovery;
pub use hestia_nvhttp as nvhttp;
pub use hestia_stream as stream;
pub use hostaddr::HostAddress;
pub use presets::Preset;
pub use settings::{
    AudioLayout, CodecPreference, FramePacing, LaunchMode, StreamOrientation, StreamSettings, TouchMode, VideoScale,
};
pub use store::{DebugOptions, Store};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Crypto(#[from] hestia_crypto::CryptoError),
    #[error(transparent)]
    Host(#[from] hestia_nvhttp::NvHttpError),
    #[error(transparent)]
    Api(#[from] hestia_api::ApiError),
    #[error("unknown host {0}")]
    UnknownHost(String),
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Longest name kept for a host on this device ([`HostRecord::display_name`]).
pub const MAX_HOST_NAME_CHARS: usize = 80;

/// A host as remembered on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRecord {
    /// The host's GameStream unique id once known; a placeholder before the
    /// first successful `/serverinfo`.
    pub id: String,
    pub name: String,
    /// Name given on this device; discovery and `/serverinfo` only change
    /// `name`. See [`HostRecord::display_name`].
    pub display_name: Option<String>,
    /// Address in use (IP or DNS name): the last one that answered.
    pub address: String,
    pub http_port: u16,
    /// Every address the host is known by, most recently used first, the one
    /// in use included: LAN, VPN (Tailscale…), DNS names. All are tried on
    /// each check; the pairing belongs to the host, not to an address.
    pub addresses: Vec<HostAddress>,
    pub https_port: u16,
    /// Pinned host certificate (PEM), present once paired.
    pub server_cert_pem: Option<String>,
    pub mac: Option<String>,
    /// Added by hand rather than discovered.
    pub manual: bool,
    /// Unix seconds of the last successful contact.
    pub last_seen: Option<u64>,
    /// Stream profile this host launches with (by name); the regular
    /// settings when unset or gone.
    pub profile: Option<String>,
    /// This host's stream profiles (each host has its own).
    pub profiles: Vec<profiles::StreamProfile>,
    /// This host's quality settings when it launches with no profile; the
    /// device's settings when unset.
    pub default_profile: Option<profiles::StreamProfile>,
}

impl Default for HostRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            display_name: None,
            address: String::new(),
            http_port: DEFAULT_HTTP_PORT,
            addresses: Vec::new(),
            https_port: DEFAULT_HTTPS_PORT,
            server_cert_pem: None,
            mac: None,
            manual: false,
            last_seen: None,
            profile: None,
            profiles: Vec::new(),
            default_profile: None,
        }
    }
}

/// Id of a record whose host hasn't answered yet.
const PENDING_PREFIX: &str = "pending:";

impl HostRecord {
    /// The name to show: the one given on this device, else the host's
    /// own, else its address.
    pub fn display_name(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .or_else(|| (!self.name.is_empty()).then_some(self.name.as_str()))
            .unwrap_or(&self.address)
    }

    pub fn is_paired(&self) -> bool {
        self.server_cert_pem.is_some()
    }

    fn server_cert_der(&self) -> Option<Vec<u8>> {
        self.server_cert_pem
            .as_deref()
            .and_then(|p| pem::decode("CERTIFICATE", p))
    }

    /// Whether the host's unique id is known yet.
    fn has_unique_id(&self) -> bool {
        !self.id.starts_with(PENDING_PREFIX)
    }

    /// The address in use.
    pub fn active_address(&self) -> HostAddress {
        HostAddress::new(self.address.clone(), self.http_port)
    }

    /// Every address to try, the one in use first. Records saved before
    /// `addresses` existed only have `address`.
    pub fn known_addresses(&self) -> Vec<HostAddress> {
        let mut list = self.addresses.clone();
        hostaddr::promote(&mut list, self.active_address());
        list
    }

    /// Whether `address` (IP or name, port aside) is one of this host's.
    pub fn knows(&self, address: &str) -> bool {
        self.known_addresses().iter().any(|a| a.is(address))
    }

    fn sync_addresses(&mut self) {
        let active = self.active_address();
        hostaddr::promote(&mut self.addresses, active);
    }

    /// Makes `address` the one in use.
    fn set_active(&mut self, address: HostAddress) {
        self.address = address.address.clone();
        self.http_port = address.http_port;
        hostaddr::promote(&mut self.addresses, address);
    }

    /// Adds an address the host can be reached at. Returns whether it is new
    /// (or its port changed).
    fn remember_address(&mut self, address: HostAddress) -> bool {
        self.sync_addresses();
        if address.is(&self.address) {
            self.http_port = address.http_port;
        }
        hostaddr::remember(&mut self.addresses, address)
    }

    /// Drops an address that now belongs to another host. The address in use
    /// moves to the next one; the last address is kept so the record stays
    /// usable (forgetting the host is the user's call).
    fn forget_address(&mut self, address: &str) {
        self.sync_addresses();
        if self.addresses.len() < 2 {
            return;
        }
        let was_active = self.active_address().is(address);
        hostaddr::forget(&mut self.addresses, address);
        if was_active && let Some(next) = self.addresses.first().cloned() {
            self.set_active(next);
        }
    }

    /// Folds in another record of the same host (same unique id): its
    /// addresses, and its pairing, MAC and profile when this one lacks them.
    fn absorb(&mut self, other: HostRecord) {
        self.sync_addresses();
        hostaddr::union(&mut self.addresses, &other.known_addresses());
        self.server_cert_pem = self.server_cert_pem.take().or(other.server_cert_pem);
        self.mac = self.mac.take().or(other.mac);
        self.profile = self.profile.take().or(other.profile);
        self.display_name = self.display_name.take().or(other.display_name);
        if self.default_profile.is_none() {
            self.default_profile = other.default_profile;
        }
        for profile in other.profiles {
            if profiles::find(&self.profiles, &profile.name).is_none() {
                profiles::upsert(&mut self.profiles, profile);
            }
        }
        self.manual |= other.manual;
        self.last_seen = self.last_seen.max(other.last_seen);
        if self.name.is_empty() {
            self.name = other.name;
        }
    }
}

/// Stores `record`, whose id may just have changed from `old_id`. If another
/// record already has that id (the same PC discovered and added by hand, or
/// known by another address), the two become one, in the older one's place.
fn store_merged(hosts: &mut Vec<HostRecord>, old_id: &str, mut record: HostRecord) -> HostRecord {
    let own = hosts.iter().position(|h| h.id == old_id);
    let other = hosts.iter().position(|h| h.id == record.id && h.id != old_id);
    match (own, other) {
        (own, Some(other)) => {
            record.absorb(hosts[other].clone());
            hosts[other] = record.clone();
            if let Some(own) = own {
                hosts.remove(own);
            }
        }
        (Some(own), None) => hosts[own] = record.clone(),
        (None, None) => hosts.push(record.clone()),
    }
    record
}

/// Profiles used to be the device's, shared by every host (`profiles.json`).
/// Now each host has its own: the hosts saved then each get a copy, which
/// keeps the profile a host was set to. Done once: the old file is then
/// renamed (kept, not deleted). A failure only costs the migration, never
/// the start; it is tried again next time.
fn adopt_legacy_profiles(store: &Store, hosts: &mut [HostRecord]) {
    if hosts.is_empty() {
        // Kept for the first host saved.
        return;
    }
    let legacy = match store.load_profiles() {
        Ok(legacy) => legacy,
        Err(e) => {
            warn!("ignoring unreadable stream profiles: {e}");
            return;
        }
    };
    if copy_legacy_profiles(hosts, &legacy) {
        if let Err(e) = store.save_hosts(hosts) {
            warn!("could not move the stream profiles to the hosts: {e}");
            return;
        }
        info!(
            "moved {} stream profiles to each of {} hosts",
            legacy.len(),
            hosts.len()
        );
    }
    if let Err(e) = store.archive_legacy_profiles() {
        warn!("could not archive the old stream profiles: {e}");
    }
}

/// [`adopt_legacy_profiles`] in memory: hosts without profiles of their own
/// get `legacy`. Returns whether any host changed.
fn copy_legacy_profiles(hosts: &mut [HostRecord], legacy: &[profiles::StreamProfile]) -> bool {
    if legacy.is_empty() {
        return false;
    }
    let mut changed = false;
    for host in hosts.iter_mut().filter(|h| h.profiles.is_empty()) {
        host.profiles = legacy.to_vec();
        changed = true;
    }
    changed
}

/// What mDNS discovery taught us about a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryNote {
    /// Id of the matching record.
    pub id: String,
    /// The record is new, or gained an address (worth checking it again).
    pub learned_address: bool,
}

/// After the first address of a host answers, how long to wait for a better
/// one (see [`hostaddr::pick`]).
const PROBE_GRACE: Duration = Duration::from_millis(250);

/// Addresses off the LAN are asked this long after the LAN ones. On Android
/// the route hook binds the whole process to a network right before each
/// connection, so the LAN connections must have their sockets before a VPN
/// or internet one rebinds to the default network. It also gives the LAN a
/// head start.
const OFF_LAN_DELAY: Duration = Duration::from_millis(150);

/// The result of asking every address of a host for `/serverinfo`.
struct Probe {
    /// The answer to use: the address, its client and what it said.
    answer: Option<(HostAddress, NvHttpClient, ServerInfo)>,
    /// Addresses now answering as another host we know.
    taken: Vec<String>,
    /// Why nothing usable answered.
    error: Option<String>,
}

/// Live view of a host after talking to it.
#[derive(Debug, Clone)]
pub struct HostSnapshot {
    pub record: HostRecord,
    pub online: bool,
    /// The host still accepts our certificate.
    pub paired: bool,
    pub info: Option<ServerInfo>,
    /// Present when the host is Hermes (Hestia protocol v1) and we are paired.
    pub capabilities: Option<Capabilities>,
    /// Hermes readiness preflight, runtime status and dependencies, when the
    /// host offers them to this client.
    pub diagnostics: Option<hestia_api::HostDiagnostics>,
    pub error: Option<String>,
}

/// Everything the streaming layer needs once `/launch` succeeded.
#[derive(Debug, Clone)]
pub struct LaunchedSession {
    pub host: HostRecord,
    pub request: LaunchRequest,
    pub response: LaunchResponse,
    pub server_info: ServerInfo,
    /// Hermes session preparation result, when the host supports it.
    pub prepared: Option<SessionPrepareResponse>,
    /// Why the capability check or session preparation failed. The app
    /// was launched in plain GameStream mode instead.
    pub prepare_error: Option<String>,
    /// Settings in effect when the app was launched.
    pub settings: StreamSettings,
}

/// Owns the host-side preparation while the data plane is being opened.
pub struct StreamHost {
    // Keep the already paired client so cleanup needs only one API call.
    hermes: Option<NvHttpClient>,
}

impl StreamHost {
    pub fn connect(
        self,
        config: stream::StreamConfig,
        video: Box<dyn stream::VideoSink>,
        audio: Box<dyn stream::AudioSink>,
        listener: Arc<dyn stream::SessionListener>,
    ) -> std::result::Result<ManagedStream, stream::StreamError> {
        let stream = stream::StreamSession::start(config, video, audio, listener)?;
        Ok(ManagedStream { _host: self, stream })
    }
}

impl Drop for StreamHost {
    fn drop(&mut self) {
        if let Some(client) = &self.hermes {
            release_hermes_session(client);
        }
    }
}

/// A stream's final network counters ([`ManagedStream::stop_with_stats`]).
pub struct SessionTotals {
    pub video: Option<report::VideoTransportStats>,
    pub audio: Option<report::AudioTransportStats>,
}

/// A local stream and the Hermes resources it owns. Field order releases the
/// host resources before tearing down the GameStream connection.
pub struct ManagedStream {
    _host: StreamHost,
    stream: stream::StreamSession,
}

/// The agreed media formats and encryption state, independent of the handshake.
pub struct StreamDetails {
    pub video_format: stream::VideoFormat,
    pub audio_format: stream::AudioFormat,
    pub encryption_enabled: u32,
    /// `x-ss-general.featureFlags` from the host (`stream::sdp::SS_FF_*`).
    pub host_feature_flags: u32,
}

impl StreamDetails {
    /// The host takes pen and touch events.
    pub fn pen_and_touch(&self) -> bool {
        self.host_feature_flags & stream::sdp::SS_FF_PEN_TOUCH_EVENTS != 0
    }
}

impl ManagedStream {
    pub fn details(&self) -> StreamDetails {
        let details = self.stream.negotiated();
        StreamDetails {
            video_format: details.video_format,
            audio_format: details.audio_format,
            encryption_enabled: details.encryption_enabled,
            host_feature_flags: details.host_feature_flags,
        }
    }

    pub fn report_negotiation(&self, report: &mut report::StreamReport) {
        add_negotiated_to_report(self.stream.negotiated(), report);
    }

    pub fn send_input(&self, event: &stream::InputEvent) {
        self.stream.send_input(event);
    }

    pub fn request_keyframe(&self) {
        self.stream.request_keyframe();
    }

    pub fn stop(self) {
        drop(self);
    }

    /// Snapshot final counters after the receive and control workers have stopped.
    pub fn stop_with_stats(self) -> SessionTotals {
        let video = self.stream.video_counters();
        let audio = self.stream.audio_counters();
        self.stop();
        SessionTotals {
            video: video.as_deref().map(report::VideoTransportStats::from),
            audio: audio.as_deref().map(report::AudioTransportStats::from),
        }
    }
}

fn release_hermes_session(client: &NvHttpClient) {
    match HestiaApi::new(client).stop_session() {
        Ok(()) => info!("Hermes session released"),
        Err(e) => warn!("Hermes session release failed: {e}"),
    }
}

impl LaunchedSession {
    /// Adds what was asked for and what `/launch` (and Hermes session
    /// preparation) answered to a session report.
    pub fn add_to_report(&self, r: &mut report::StreamReport) {
        self.settings.add_to_report(r);
        let info = &self.server_info;
        r.field("Host", "name", &self.host.name);
        r.field("Host", "address", &self.host.address);
        r.field("Host", "version", &info.app_version);
        if !info.gfe_version.is_empty() {
            r.field("Host", "GFE version", &info.gfe_version);
        }
        r.field(
            "Host",
            "codecs",
            hestia_nvhttp::codecs::describe(info.codec_mode_support).join(", "),
        );
        r.field("Launch", "session URL", &self.response.session_url);
        r.field("Launch", "resumed", self.response.resumed);
        r.field(
            "Launch",
            "requested",
            format!("{}x{} @ {}", self.request.width, self.request.height, self.request.fps),
        );
        match &self.prepared {
            Some(p) => {
                let vd = &p.virtual_display;
                r.field("Launch", "Hermes session", &p.session_id);
                r.field(
                    "Launch",
                    "Hermes display",
                    format!(
                        "{} via {}, {}x{} @ {}{}",
                        vd.name,
                        vd.backend,
                        vd.width,
                        vd.height,
                        vd.fps,
                        if vd.hdr { ", HDR" } else { "" }
                    ),
                );
                for w in &p.warnings {
                    r.field("Launch", "Hermes warning", w);
                }
            }
            None => match &self.prepare_error {
                Some(why) => r.field(
                    "Launch",
                    "Hermes session",
                    format!("not prepared: {why} (plain GameStream used)"),
                ),
                None => r.field("Launch", "Hermes session", "not prepared (Hestia API unavailable)"),
            },
        }
        r.event(if self.response.resumed {
            "app resumed on the host"
        } else {
            "app launched on the host"
        });
    }

    /// Parameters for [`hestia_stream::StreamSession`] matching this launch.
    /// `decoder_formats` are the formats the chosen decoder handles; they are
    /// reordered by the user's codec preference.
    pub fn stream_config(&self, decoder_formats: &[hestia_stream::VideoFormat]) -> Result<hestia_stream::StreamConfig> {
        use hestia_stream::{SessionUrl, StreamConfig, VideoCodec, is_private_address};

        let session_url = SessionUrl::parse(&self.response.session_url, &self.host.address)
            .map_err(|e| CoreError::Invalid(format!("bad session URL {:?}: {e}", self.response.session_url)))?;
        let remote_ip = session_url.host.parse::<std::net::IpAddr>().ok().or_else(|| {
            use std::net::ToSocketAddrs;
            (session_url.host.as_str(), session_url.port)
                .to_socket_addrs()
                .ok()?
                .next()
                .map(|a| a.ip())
        });
        // On a network we're attached to (an IPv6 global address of the home
        // network included), the stream stays local.
        let streaming_remotely =
            remote_ip.is_some_and(|ip| !is_private_address(ip) && !netif::on_link(ip, &netif::local_addresses()));

        let preferred = match self.settings.codec {
            CodecPreference::Auto => None,
            CodecPreference::H264 => Some(VideoCodec::H264),
            CodecPreference::Hevc => Some(VideoCodec::Hevc),
            CodecPreference::Av1 => Some(VideoCodec::Av1),
            CodecPreference::PyroWave => Some(VideoCodec::PyroWave),
        };
        let mut video_formats: Vec<_> = decoder_formats
            .iter()
            .copied()
            .filter(|f| (f.codec == VideoCodec::PyroWave) == (preferred == Some(VideoCodec::PyroWave)))
            .filter(|f| self.request.hdr || !f.ten_bit)
            .collect();
        if let Some(codec) = preferred {
            video_formats.sort_by_key(|f| f.codec != codec);
        }
        if video_formats.is_empty() {
            return Err(CoreError::Invalid("the decoder supports no usable video format".into()));
        }

        Ok(StreamConfig {
            session_url,
            server_codec_modes: self.server_info.codec_mode_support,
            width: self.request.width,
            height: self.request.height,
            fps: self.request.fps,
            bitrate_kbps: self.settings.bitrate_kbps,
            packet_size: if streaming_remotely {
                StreamConfig::WAN_PACKET_SIZE
            } else {
                StreamConfig::LAN_PACKET_SIZE
            },
            video_formats,
            audio_channels: self.request.audio.channel_count() as u8,
            audio_channel_mask: self.request.audio.channel_mask(),
            remote_input_key: self.request.remote_input_key,
            remote_input_key_id: self.request.remote_input_key_id,
            streaming_remotely,
            show_video_through_loss: self.settings.show_video_through_loss,
        })
    }
}

/// Hermes v1 validates `client.platform` against this list; anything else
/// (Android included) must be sent as `unknown`.
fn hermes_platform(platform: &str) -> &'static str {
    match platform {
        "linux" => "linux",
        "windows" => "windows",
        "macos" => "macos",
        "steamdeck" => "steamdeck",
        _ => "unknown",
    }
}

/// A missing API endpoint is an ordinary GameStream fallback. Keep probe
/// failures and incompatible responses available to the launch report.
fn session_prepare_capabilities(probe: hestia_api::Result<Capabilities>) -> Result<Option<Capabilities>, String> {
    match probe {
        Ok(caps) if caps.is_compatible() => Ok(Some(caps)),
        Ok(caps) => Err(format!(
            "incompatible Hestia capabilities (host protocol {}, supported client versions {}–{})",
            caps.hestia_protocol, caps.min_client_protocol, caps.max_client_protocol,
        )),
        Err(error) if error.is_unsupported() => Ok(None),
        Err(error) => Err(format!("Hestia capabilities check failed: {error}")),
    }
}

/// The mode must be one Hermes advertises (frame rates are a fixed list).
fn check_prepare_limits(limits: &hestia_api::Limits, s: &StreamSettings) -> std::result::Result<(), String> {
    if limits.max_width > 0 && (s.width > limits.max_width || s.height > limits.max_height) {
        return Err(format!(
            "{}x{} is above the host's limit of {}x{}",
            s.width, s.height, limits.max_width, limits.max_height
        ));
    }
    if !limits.supported_fps.is_empty() && !limits.supported_fps.contains(&s.fps) {
        let list: Vec<String> = limits.supported_fps.iter().map(u32::to_string).collect();
        return Err(format!(
            "Hermes doesn't offer {} fps (it offers {})",
            s.fps,
            list.join(", ")
        ));
    }
    if !(1..=500).contains(&s.scale_factor) {
        return Err(format!("scale factor {}% is outside 1-500%", s.scale_factor));
    }
    Ok(())
}

/// Facts about this device that go into Hermes' session preparation.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub platform: &'static str,
    pub display_width: u32,
    pub display_height: u32,
    pub refresh_rate: u32,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            platform: if cfg!(target_os = "android") {
                "android"
            } else if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else if cfg!(target_os = "macos") {
                "macos"
            } else {
                "unknown"
            },
            display_width: 1920,
            display_height: 1080,
            refresh_rate: 60,
        }
    }
}

pub struct HestiaClient {
    store: Store,
    identity: Arc<ClientIdentity>,
    hosts: Mutex<Vec<HostRecord>>,
    settings: Mutex<StreamSettings>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl HestiaClient {
    /// Opens (or initialises) the client state in `data_dir`. Generates the
    /// identity on first run, which takes a moment.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let store = Store::open(data_dir)?;
        let identity = match store.load_identity()? {
            Some(saved) => ClientIdentity::from_pem(&saved.unique_id, &saved.certificate_pem, &saved.private_key_pem)?,
            None => {
                info!("generating a new client identity");
                let identity = ClientIdentity::generate()?;
                store.save_identity(&store::StoredIdentity {
                    unique_id: identity.unique_id().to_owned(),
                    certificate_pem: identity.cert_pem(),
                    private_key_pem: identity.key_pem(),
                })?;
                identity
            }
        };
        let mut hosts = store.load_hosts()?;
        let settings = store.load_settings().unwrap_or_else(|e| {
            warn!("ignoring unreadable settings: {e}");
            StreamSettings::default()
        });
        adopt_legacy_profiles(&store, &mut hosts);
        Ok(Self {
            store,
            identity: Arc::new(identity),
            hosts: Mutex::new(hosts),
            settings: Mutex::new(settings),
        })
    }

    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    pub fn settings(&self) -> StreamSettings {
        self.settings.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_settings(&self, settings: StreamSettings) -> Result<()> {
        self.store.save_settings(&settings)?;
        *self.settings.lock().unwrap_or_else(|p| p.into_inner()) = settings;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Debug options
    // ------------------------------------------------------------------

    /// Settings → Debug, as saved; off when never saved or unreadable.
    pub fn debug_options(&self) -> DebugOptions {
        self.store.load_debug_options().unwrap_or_else(|e| {
            warn!("ignoring unreadable debug options: {e}");
            DebugOptions::default()
        })
    }

    pub fn set_debug_options(&self, options: DebugOptions) -> Result<()> {
        self.store.save_debug_options(&options)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Stream profiles
    // ------------------------------------------------------------------

    /// Host `id`'s stream profiles, by name. They belong to that host: two
    /// hosts can each have a "Couch TV" of their own.
    pub fn profiles(&self, id: &str) -> Result<Vec<profiles::StreamProfile>> {
        Ok(self.host(id)?.profiles)
    }

    /// Saves `settings`' quality side as host `id`'s profile `name`
    /// (replacing one of that name). Returns the name as kept.
    pub fn save_profile(&self, id: &str, name: &str, settings: &StreamSettings) -> Result<String> {
        let name = profiles::clean_name(name).ok_or_else(|| CoreError::Invalid("the profile needs a name".into()))?;
        self.update_host(id, |host| {
            profiles::upsert(
                &mut host.profiles,
                profiles::StreamProfile::from_settings(&name, settings),
            );
            Ok(name)
        })
    }

    /// Saves `settings`' quality side for host `id`: into the profile it
    /// launches with, or into its own defaults when it has none.
    pub fn set_host_settings(&self, id: &str, settings: &StreamSettings) -> Result<()> {
        self.update_host(id, |host| {
            // The stored name, not the choice: a choice whose profile is gone
            // must not bring it back.
            let chosen = host
                .profile
                .as_deref()
                .and_then(|name| profiles::find(&host.profiles, name))
                .map(|p| p.name.clone());
            match chosen {
                Some(name) => {
                    profiles::upsert(
                        &mut host.profiles,
                        profiles::StreamProfile::from_settings(&name, settings),
                    );
                }
                None => {
                    host.default_profile = Some(profiles::StreamProfile::from_settings(
                        profiles::HOST_DEFAULTS,
                        settings,
                    ));
                }
            }
            Ok(())
        })
    }

    /// Deletes host `id`'s profile `name`; the host goes back to its own
    /// defaults if it launched with it. Other hosts aren't touched.
    pub fn delete_profile(&self, id: &str, name: &str) -> Result<()> {
        self.update_host(id, |host| {
            host.profiles.retain(|p| !p.name.eq_ignore_ascii_case(name));
            if host.profile.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(name)) {
                host.profile = None;
            }
            Ok(())
        })
    }

    /// Makes host `id` launch with its profile `name` (None: its defaults).
    pub fn set_host_profile(&self, id: &str, name: Option<&str>) -> Result<()> {
        self.update_host(id, |host| {
            let chosen = name
                .map(|name| {
                    profiles::find(&host.profiles, name)
                        .map(|p| p.name.clone())
                        .ok_or_else(|| CoreError::Invalid(format!("this host has no profile “{name}”")))
                })
                .transpose()?;
            host.profile = chosen;
            Ok(())
        })
    }

    /// What host `id` launches with: the device's settings with the quality
    /// side of its profile (or of its own defaults) over them, and the
    /// profile's name. Touch, decoding and the rest of the device's choices
    /// always come from the device's settings.
    pub fn settings_for_host(&self, id: &str) -> (StreamSettings, Option<String>) {
        let settings = self.settings();
        let Ok(host) = self.host(id) else {
            return (settings, None);
        };
        if let Some(profile) = host
            .profile
            .as_deref()
            .and_then(|name| profiles::find(&host.profiles, name))
        {
            return (profile.apply(&settings), Some(profile.name.clone()));
        }
        match &host.default_profile {
            Some(defaults) => (defaults.apply(&settings), None),
            None => (settings, None),
        }
    }

    // ------------------------------------------------------------------
    // Host registry
    // ------------------------------------------------------------------

    pub fn hosts(&self) -> Vec<HostRecord> {
        self.lock_hosts().clone()
    }

    pub fn host(&self, id: &str) -> Result<HostRecord> {
        self.lock_hosts()
            .iter()
            .find(|h| h.id == id)
            .cloned()
            .ok_or_else(|| CoreError::UnknownHost(id.to_owned()))
    }

    fn lock_hosts(&self) -> std::sync::MutexGuard<'_, Vec<HostRecord>> {
        self.hosts.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update_hosts<R>(&self, f: impl FnOnce(&mut Vec<HostRecord>) -> R) -> Result<R> {
        let mut hosts = self.lock_hosts();
        let result = f(&mut hosts);
        self.store.save_hosts(&hosts)?;
        Ok(result)
    }

    /// Changes host `id` and saves the list. Nothing is saved when there is
    /// no such host or `f` fails (it must fail before changing anything).
    fn update_host<R>(&self, id: &str, f: impl FnOnce(&mut HostRecord) -> Result<R>) -> Result<R> {
        let mut hosts = self.lock_hosts();
        let host = hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| CoreError::UnknownHost(id.to_owned()))?;
        let result = f(host)?;
        self.store.save_hosts(&hosts)?;
        Ok(result)
    }

    /// Adds a host by address (`192.168.0.10`, `pc.local:47989`, `[fe80::1]:47989`)
    /// and contacts it.
    pub fn add_manual(&self, address: &str) -> Result<HostSnapshot> {
        let (host, port) = address::parse(address)?;
        let typed = HostAddress::new(host.clone(), port);
        // An address we already know: check that host. A new one gets its own
        // record until the host answers; if it turns out to be a host we know
        // (its Tailscale address, a new DHCP lease…), `refresh` merges the two
        // and the pairing carries over.
        let existing = self.update_hosts(|hosts| {
            let h = hosts.iter_mut().find(|h| h.knows(&host))?;
            h.remember_address(typed.clone());
            Some(h.id.clone())
        })?;
        let id = match existing {
            Some(id) => id,
            None => {
                let record = HostRecord {
                    id: format!("{PENDING_PREFIX}{host}:{port}"),
                    name: host.clone(),
                    address: host,
                    http_port: port,
                    addresses: vec![typed],
                    manual: true,
                    ..Default::default()
                };
                let id = record.id.clone();
                self.update_hosts(|hosts| hosts.push(record))?;
                id
            }
        };
        let snapshot = self.refresh(&id)?;
        if !snapshot.online {
            // Keep it so the user can retry, but report the failure.
            return Err(CoreError::Invalid(
                snapshot.error.unwrap_or_else(|| "host did not answer".into()),
            ));
        }
        Ok(snapshot)
    }

    /// Records a host seen via mDNS: a new record, or its addresses added to
    /// the matching one (they are all tried on the next check, so a new DHCP
    /// lease doesn't lose the pairing).
    pub fn note_discovered(&self, found: &DiscoveredHost) -> Result<DiscoveryNote> {
        let addresses: Vec<HostAddress> = found
            .addresses
            .iter()
            .filter(|ip| hostaddr::worth_keeping(**ip))
            .map(|ip| HostAddress::new(ip.to_string(), found.port))
            .collect();
        let Some(primary) = addresses.first().cloned() else {
            return Err(CoreError::InvalidAddress(format!(
                "{} has no usable address",
                found.name
            )));
        };
        self.update_hosts(|hosts| {
            if let Some(h) = hosts
                .iter_mut()
                .find(|h| addresses.iter().any(|a| h.knows(&a.address)) || (!h.name.is_empty() && h.name == found.name))
            {
                let mut learned = false;
                for a in &addresses {
                    learned |= h.remember_address(a.clone());
                }
                return DiscoveryNote {
                    id: h.id.clone(),
                    learned_address: learned,
                };
            }
            let record = HostRecord {
                id: format!("{PENDING_PREFIX}{}:{}", primary.address, primary.http_port),
                name: found.name.clone(),
                address: primary.address.clone(),
                http_port: primary.http_port,
                addresses: addresses.clone(),
                ..Default::default()
            };
            let id = record.id.clone();
            hosts.push(record);
            DiscoveryNote {
                id,
                learned_address: true,
            }
        })
    }

    /// Sends Wake-on-LAN to a host whose MAC address is known (from an
    /// earlier `/serverinfo`). Returns how many packets went out.
    pub fn wake(&self, id: &str) -> Result<usize> {
        let record = self.host(id)?;
        let mac = record
            .mac
            .as_deref()
            .and_then(wol::parse_mac)
            .ok_or_else(|| CoreError::Invalid(format!("no MAC address known for {}", record.name)))?;
        // Every known IPv4 address gets its directed broadcast: the address in
        // use may be the VPN one while we're back on the host's LAN. IPv6
        // goes to the all-nodes group on each interface instead.
        let ips: Vec<std::net::IpAddr> = record
            .known_addresses()
            .iter()
            .filter_map(HostAddress::ip)
            .filter(std::net::IpAddr::is_ipv4)
            .collect();
        let mut sent = 0;
        let mut last_err = None;
        let targets = if ips.is_empty() {
            vec![None]
        } else {
            ips.into_iter().map(Some).collect()
        };
        for ip in targets {
            match wol::wake(mac, ip, hestia_nvhttp::http::route) {
                Ok(n) => sent += n,
                Err(e) => last_err = Some(e),
            }
        }
        match wol::wake_ipv6(mac, &netif::ipv6_interfaces(&netif::local_addresses())) {
            Ok(n) => sent += n,
            Err(e) => last_err = Some(e),
        }
        if let (0, Some(e)) = (sent, last_err) {
            return Err(CoreError::Invalid(format!("Wake-on-LAN: {e}")));
        }
        info!("Wake-on-LAN sent to {} ({sent} packets)", record.name);
        Ok(sent)
    }

    /// Renames host `id` on this device and makes `address` the one it is
    /// reached at. An empty name, or the host's own, goes back to the name
    /// the host reports (and follows it). The id, the pairing and the other
    /// addresses stay; nothing changes on the host.
    pub fn edit_host_details(&self, id: &str, name: &str, address: &str) -> Result<()> {
        let name = name.trim();
        if name.chars().count() > MAX_HOST_NAME_CHARS {
            return Err(CoreError::Invalid(format!(
                "the name can be up to {MAX_HOST_NAME_CHARS} characters"
            )));
        }
        let (host_address, port) = address::parse(address)?;
        self.update_host(id, |host| {
            host.display_name = (!name.is_empty() && name != host.name).then(|| name.to_owned());
            // The address in use so far stays one of the host's (records
            // saved before `addresses` only have it in `address`).
            host.sync_addresses();
            host.set_active(HostAddress::new(host_address, port));
            host.manual = true;
            Ok(())
        })
    }

    /// Forgets a host locally (the host keeps its own paired-client entry
    /// until removed in its web UI).
    pub fn forget(&self, id: &str) -> Result<()> {
        let mut hosts = self.lock_hosts();
        let mut saved = hosts.clone();
        saved.retain(|h| h.id != id);
        self.store.save_hosts(&saved)?;
        *hosts = saved;
        Ok(())
    }

    /// A ready-to-use GameStream client for a host, at the address in use.
    pub fn nvhttp(&self, record: &HostRecord) -> NvHttpClient {
        self.nvhttp_at(record, &record.active_address())
    }

    fn nvhttp_at(&self, record: &HostRecord, address: &HostAddress) -> NvHttpClient {
        NvHttpClient::new(address.address.clone(), address.http_port, self.identity.clone())
            .with_https_port(record.https_port)
            .with_server_cert(record.server_cert_der())
    }

    /// Asks every known address of the host for `/serverinfo` at once and
    /// keeps the best answer (see [`hostaddr::pick`]): the LAN when we're on
    /// it, the VPN or the internet otherwise.
    fn probe(&self, record: &HostRecord) -> Probe {
        let candidates = record.known_addresses();
        let pinned = record.is_paired();
        // Unique ids of the other hosts we know: an address answering with one
        // of those now belongs to that host.
        let others: Vec<String> = if record.has_unique_id() {
            self.lock_hosts()
                .iter()
                .filter(|h| h.id != record.id && h.has_unique_id())
                .map(|h| h.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        // What each address offers: the kind of network (what shares a link
        // with this device is LAN) and its family (IPv6 first).
        let locals = netif::local_addresses();
        let offers: Vec<(hostaddr::AddressKind, u8)> = candidates
            .iter()
            .map(|a| (a.kind_with(|ip| netif::on_link(ip, &locals)), a.family_rank()))
            .collect();
        let best = offers
            .iter()
            .min()
            .copied()
            .unwrap_or((hostaddr::AddressKind::Other, u8::MAX));

        let (tx, rx) = mpsc::channel();
        for (index, address) in candidates.iter().enumerate() {
            let mut client = self.nvhttp_at(record, address);
            let delay =
                (offers[index].0 != hostaddr::AddressKind::Lan && candidates.len() > 1).then_some(OFF_LAN_DELAY);
            let tx = tx.clone();
            let spawned = std::thread::Builder::new().name("hestia-probe".into()).spawn(move || {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                let result = match client.server_info() {
                    Ok(info) => Ok((client, info)),
                    Err(e) => Err(e.to_string()),
                };
                // The receiver is gone once a better address answered.
                let _ = tx.send((index, result));
            });
            if let Err(e) = spawned {
                warn!("could not check {}: {e}", address.address);
            }
        }
        drop(tx);

        let mut answers: Vec<(hostaddr::Answer, NvHttpClient, ServerInfo)> = Vec::new();
        let mut errors: Vec<(usize, String)> = Vec::new();
        let mut taken = Vec::new();
        let mut deadline: Option<Instant> = None;
        loop {
            let message = match deadline {
                None => rx.recv().ok(),
                Some(deadline) => rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok(),
            };
            let Some((index, result)) = message else { break };
            let address = &candidates[index];
            let (client, info) = match result {
                Ok(answer) => answer,
                Err(e) => {
                    log::debug!("{} at {}: {e}", record.name, address.address);
                    errors.push((index, e));
                    continue;
                }
            };
            if !info.unique_id.is_empty() && others.contains(&info.unique_id) {
                info!(
                    "{} now answers as another known host ({}); forgetting it for {}",
                    address.address, info.hostname, record.name
                );
                taken.push(address.address.clone());
                continue;
            }
            let answer = hostaddr::Answer {
                index,
                kind: offers[index].0,
                family: offers[index].1,
                verified: info.paired,
                same_host: !record.has_unique_id() || info.unique_id.is_empty() || info.unique_id == record.id,
            };
            let ideal = answer.is_ideal(pinned, best);
            if answer.same_host {
                deadline.get_or_insert_with(|| Instant::now() + PROBE_GRACE);
            }
            answers.push((answer, client, info));
            if ideal {
                break;
            }
        }

        let picked = hostaddr::pick(&answers.iter().map(|(a, ..)| *a).collect::<Vec<_>>());
        let answer = picked.map(|i| {
            let (answer, client, info) = answers.swap_remove(i);
            let address = candidates[answer.index].clone();
            if answer.index != 0 {
                info!("{} answers at {} now", record.name, address.address);
            }
            (address, client, info)
        });
        let error = if answer.is_some() {
            None
        } else if let Some(other) = answers.first() {
            Some(format!(
                "{} now answers as a different host ({})",
                candidates[other.0.index].address, other.2.hostname
            ))
        } else {
            // The address in use says the most about why the host is offline.
            Some(
                errors
                    .into_iter()
                    .min_by_key(|(index, _)| *index)
                    .map(|(_, e)| e)
                    .unwrap_or_else(|| "host did not answer".into()),
            )
        };
        Probe { answer, taken, error }
    }

    /// Applies a probe only while the saved host is still at the address it
    /// used when the check started, including when none of its addresses answered.
    fn save_probe(&self, checked: &HostRecord, probe: &Probe) -> Result<HostRecord> {
        let mut hosts = self.lock_hosts();
        let mut updated = hosts
            .iter()
            .find(|host| host.id == checked.id)
            .cloned()
            .ok_or_else(|| CoreError::UnknownHost(checked.id.clone()))?;
        if updated.active_address() != checked.active_address() {
            return Err(CoreError::Invalid("host address changed during the check".into()));
        }
        if probe.answer.is_none() && probe.taken.is_empty() {
            return Ok(updated);
        }

        for taken in &probe.taken {
            updated.forget_address(taken);
        }
        if let Some((address, client, info)) = &probe.answer {
            updated.set_active(address.clone());
            if !info.unique_id.is_empty() {
                updated.id = info.unique_id.clone();
            }
            if !info.hostname.is_empty() {
                updated.name = info.hostname.clone();
            }
            updated.https_port = client.https_port();
            updated.mac = info.mac.clone().or(updated.mac);
            updated.last_seen = Some(now_secs());
        }

        // A new address of a known host can merge into its paired record.
        let mut saved = hosts.clone();
        let updated = store_merged(&mut saved, &checked.id, updated);
        self.store.save_hosts(&saved)?;
        *hosts = saved;
        Ok(updated)
    }

    /// Talks to the host: `/serverinfo` on every address it is known by, and
    /// for paired Hermes hosts the Hestia capabilities. Updates the stored
    /// record (id, name, ports, the address in use...), merging it with
    /// another record of the same host.
    pub fn refresh(&self, id: &str) -> Result<HostSnapshot> {
        let record = self.host(id)?;
        let probe = self.probe(&record);
        let record = self.save_probe(&record, &probe)?;
        let Some((_, mut client, mut info)) = probe.answer else {
            return Ok(HostSnapshot {
                paired: record.is_paired(),
                record,
                online: false,
                info: None,
                capabilities: None,
                diagnostics: None,
                error: probe.error,
            });
        };

        if record.is_paired() && !client.is_pinned() {
            // We only learned this is a paired host from the answer (over
            // plain HTTP): ask again with its certificate for the real status.
            client.set_server_cert(record.server_cert_der());
            match client.server_info() {
                Ok(fresh) => info = fresh,
                Err(e) => info.secure_error = Some(e.to_string()),
            }
        }

        // When only the HTTPS port failed (busy host, blocked port), plain
        // HTTP can't tell whether we're paired: keep what we know.
        let paired = record.is_paired() && (info.paired || info.secure_error.is_some());
        let capabilities = if paired {
            match HestiaApi::new(&client).capabilities() {
                Ok(caps) if caps.is_compatible() => Some(caps),
                Ok(caps) => {
                    info!(
                        "{} speaks Hestia protocol {}, which we don't",
                        record.name, caps.hestia_protocol
                    );
                    None
                }
                Err(e) => {
                    log::debug!("no Hestia API on {}: {e}", record.name);
                    None
                }
            }
        } else {
            None
        };
        // Readiness preflight for the host health badge (best effort: the
        // endpoint can be permission-gated or absent on older Hermes).
        let diagnostics = capabilities
            .as_ref()
            .and_then(|_| match HestiaApi::new(&client).host_diagnostics() {
                Ok(d) => Some(d),
                Err(e) => {
                    log::debug!("Hermes diagnostics unavailable on {}: {e}", record.name);
                    None
                }
            });

        Ok(HostSnapshot {
            record,
            online: true,
            paired,
            info: Some(info),
            capabilities,
            diagnostics,
            error: None,
        })
    }

    // ------------------------------------------------------------------
    // Pairing
    // ------------------------------------------------------------------

    /// Pairs with the host. With `otp_passphrase` = None, `pin` must be typed
    /// into the host's web UI while this call blocks. With a passphrase, `pin`
    /// and passphrase are the one-time values the host displayed.
    ///
    /// Returns the outcome and the host's id afterwards (a placeholder id is
    /// replaced by the host's unique id once known).
    pub fn pair(&self, id: &str, pin: &PairingPin, otp_passphrase: Option<&str>) -> Result<(PairingOutcome, String)> {
        let record = self.host(id)?;
        let mut client = self.nvhttp(&record);
        client.set_server_cert(None);
        let info = client.server_info()?;
        let outcome = client.pair(info.major_version(), pin, otp_passphrase)?;
        let mut final_id = record.id.clone();
        if let PairingOutcome::Paired { server_cert_der } = &outcome {
            let cert_pem = pem::encode("CERTIFICATE", server_cert_der);
            let https_port = client.https_port();
            let host_id = record.id.clone();
            final_id = self.update_hosts(|hosts| {
                let Some(mut paired) = hosts.iter().find(|h| h.id == host_id).cloned() else {
                    return host_id.clone();
                };
                paired.server_cert_pem = Some(cert_pem);
                paired.https_port = https_port;
                if !info.unique_id.is_empty() {
                    paired.id = info.unique_id.clone();
                }
                if !info.hostname.is_empty() {
                    paired.name = info.hostname.clone();
                }
                // Another record of the same host (an older pairing, another
                // address of it) merges in; this pairing wins.
                store_merged(hosts, &host_id, paired).id
            })?;
        }
        Ok((outcome, final_id))
    }

    /// Asks the host to drop a pending pairing attempt (best effort).
    pub fn cancel_pairing(&self, id: &str) {
        if let Ok(record) = self.host(id) {
            self.nvhttp(&record).cancel_pairing();
        }
    }

    // ------------------------------------------------------------------
    // Apps & sessions
    // ------------------------------------------------------------------

    pub fn apps(&self, id: &str) -> Result<Vec<AppInfo>> {
        let record = self.host(id)?;
        Ok(self.nvhttp(&record).app_list()?)
    }

    pub fn box_art(&self, id: &str, app_id: u32) -> Result<Vec<u8>> {
        let record = self.host(id)?;
        Ok(self.nvhttp(&record).box_art(app_id)?)
    }

    pub fn capabilities(&self, id: &str) -> Result<Capabilities> {
        let record = self.host(id)?;
        Ok(HestiaApi::new(&self.nvhttp(&record)).capabilities()?)
    }

    /// Runs `f` with a Hestia API handle for the host.
    pub fn with_api<R>(&self, id: &str, f: impl FnOnce(&HestiaApi<'_>) -> R) -> Result<R> {
        let record = self.host(id)?;
        let client = self.nvhttp(&record);
        Ok(f(&HestiaApi::new(&client)))
    }

    /// Starts (or resumes) an app with the current settings. On Hermes the
    /// session is prepared first through the Hestia API.
    pub fn launch(&self, id: &str, app_id: u32, device: &DeviceInfo) -> Result<LaunchedSession> {
        self.launch_with(id, app_id, device, self.settings())
    }

    /// Like [`launch`](Self::launch) with one-off settings (not persisted).
    pub fn launch_with(
        &self,
        id: &str,
        app_id: u32,
        device: &DeviceInfo,
        settings: StreamSettings,
    ) -> Result<LaunchedSession> {
        let record = self.host(id)?;
        if !record.is_paired() {
            return Err(CoreError::Host(hestia_nvhttp::NvHttpError::NotPaired));
        }
        let mut client = self.nvhttp(&record);
        let server_info = client.server_info()?;
        if !server_info.paired {
            return Err(CoreError::Host(hestia_nvhttp::NvHttpError::NotPaired));
        }
        if server_info.is_busy() && server_info.current_game != app_id {
            return Err(CoreError::Invalid(
                "another app is running on this host; quit it first".into(),
            ));
        }

        if settings.codec == CodecPreference::PyroWave {
            if settings.hdr {
                return Err(CoreError::Invalid(
                    "PyroWave requires SDR. Turn off HDR in stream settings.".into(),
                ));
            }
            if server_info.codec_mode_support & hestia_nvhttp::codecs::SCM_PYROWAVE == 0 {
                return Err(CoreError::Invalid(
                    "The host does not advertise PyroWave. Enable it in Hermes or choose another codec.".into(),
                ));
            }
        }

        let mut request = LaunchRequest::new(app_id, settings.width, settings.height, settings.fps);
        request.optimize_game_settings = settings.optimize_game_settings;
        request.play_audio_on_host = settings.play_audio_on_host;
        request.audio = match settings.audio {
            AudioLayout::Stereo => AudioConfig::Stereo,
            AudioLayout::Surround51 => AudioConfig::Surround51,
            AudioLayout::Surround71 => AudioConfig::Surround71,
        };
        request.hdr = settings.hdr;
        request.virtual_display = settings.virtual_display && server_info.virtual_display_capable;
        if settings.scale_factor != 100 {
            request.scale_factor = Some(settings.scale_factor);
        }

        let (prepared, prepare_error) = match self.prepare_hermes_session(&client, &settings, device, app_id) {
            Ok(prepared) => (prepared, None),
            Err(why) => {
                warn!("Hermes session not prepared, continuing with plain GameStream: {why}");
                (None, Some(why))
            }
        };

        let response = if server_info.is_busy() {
            client.resume(&request)
        } else {
            client.launch(&request)
        }
        .inspect_err(|_| {
            if prepared.is_some() {
                release_hermes_session(&client);
            }
        })?;
        info!("session started on {}: {}", record.name, response.session_url);

        Ok(LaunchedSession {
            host: record,
            request,
            response,
            server_info,
            prepared,
            prepare_error,
            settings,
        })
    }

    fn prepare_hermes_session(
        &self,
        client: &NvHttpClient,
        settings: &StreamSettings,
        device: &DeviceInfo,
        app_id: u32,
    ) -> Result<Option<SessionPrepareResponse>, String> {
        let api = HestiaApi::new(client);
        let Some(caps) = session_prepare_capabilities(api.capabilities())? else {
            return Ok(None);
        };
        // Hermes rejects (400) rather than negotiates: check what it says it
        // accepts and explain, instead of sending something it will refuse.
        check_prepare_limits(&caps.limits, settings)?;
        let features = &caps.features;
        let hdr = settings.hdr && features.hdr_mode_control;
        let request = SessionPrepareRequest {
            client: ClientDescription {
                name: "Hestia RS".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                platform: hermes_platform(device.platform).into(),
                display_width: device.display_width,
                display_height: device.display_height,
                refresh_rate: device.refresh_rate,
                hdr,
            },
            stream: StreamRequest {
                requested_width: settings.width,
                requested_height: settings.height,
                requested_fps: settings.fps,
                codec: settings.codec.api_name().into(),
                bitrate_kbps: settings.bitrate_kbps,
                hdr_mode: if hdr { "hdr" } else { "sdr" }.into(),
                scale_factor: if features.scale_factor {
                    settings.scale_factor
                } else {
                    100
                },
            },
            virtual_display: VirtualDisplayRequest {
                enabled: features.virtual_display && settings.virtual_display,
                backend: "auto".into(),
                desktop_integration: "auto".into(),
                recover_physical_monitor: features.display_recovery,
            },
            app: AppRequest {
                id: app_id.to_string(),
                launch_mode: if features.gamescope_session && settings.launch_mode == LaunchMode::Gamescope {
                    "gamescope"
                } else {
                    "normal"
                }
                .into(),
            },
        };
        match api.prepare_session(&request) {
            Ok(prepared) => {
                for warning in &prepared.warnings {
                    warn!("Hermes session warning: {warning}");
                }
                Ok(Some(prepared))
            }
            Err(e) => Err(if e.is_permission_denied() {
                "this device is not allowed to prepare sessions".to_owned()
            } else {
                e.to_string()
            }),
        }
    }

    /// Claims the preparation for a streaming launch. Dropping this before
    /// connecting also rolls back a launch cancelled by the local client.
    pub fn stream_host(&self, launch: &LaunchedSession) -> StreamHost {
        StreamHost {
            hermes: launch.prepared.as_ref().map(|_| self.nvhttp(&launch.host)),
        }
    }

    /// Hermes: releases this client's prepared session (virtual display,
    /// Gamescope session) when its stream ends. Best effort; plain hosts and
    /// errors are ignored. Only this client's session is affected.
    pub fn end_hermes_session(&self, id: &str) {
        let Ok(record) = self.host(id) else { return };
        let client = self.nvhttp(&record);
        let api = HestiaApi::new(&client);
        if api.capabilities().is_ok_and(|c| c.is_compatible()) {
            match api.stop_session() {
                Ok(()) => info!("Hermes session on {} stopped", record.name),
                Err(e) => log::debug!("Hermes session stop failed: {e}"),
            }
        }
    }

    /// The host's web UI (Hermes/Sunshine/Apollo serve it on the HTTP port + 1).
    pub fn web_ui_url(&self, id: &str) -> Result<String> {
        let record = self.host(id)?;
        let host = if record.address.contains(':') && !record.address.starts_with('[') {
            format!("[{}]", record.address)
        } else {
            record.address.clone()
        };
        Ok(format!("https://{host}:{}", record.http_port + 1))
    }

    /// Quits the running app (and, on Hermes, clears the prepared session).
    pub fn quit(&self, id: &str) -> Result<()> {
        let record = self.host(id)?;
        let client = self.nvhttp(&record);
        let api = HestiaApi::new(&client);
        if api.capabilities().is_ok_and(|c| c.is_compatible()) {
            if let Err(e) = api.stop_session() {
                log::debug!("Hestia session stop failed: {e}");
            }
        }
        client.quit_app()?;
        Ok(())
    }
}

/// Adds the stream parameters a session asked for to a report.
pub fn add_stream_config_to_report(c: &hestia_stream::StreamConfig, decoder: &str, r: &mut report::StreamReport) {
    r.field("Stream", "RTSP", &c.session_url.raw);
    r.field("Stream", "decoder", decoder);
    r.field("Stream", "offered formats", format!("{:?}", c.video_formats));
    r.field("Stream", "packet size", c.packet_size);
    r.field("Stream", "remote (WAN)", c.streaming_remotely);
}

/// Adds what the RTSP handshake settled on to a report.
fn add_negotiated_to_report(n: &hestia_stream::rtsp::Negotiated, r: &mut report::StreamReport) {
    r.field("Negotiated", "video", format!("{:?}", n.video_format));
    r.field(
        "Negotiated",
        "audio",
        format!(
            "{} ch, {} streams, {} ms packets{}",
            n.audio_format.channels,
            n.audio_format.streams,
            n.audio_packet_duration_ms,
            if n.audio_encrypted { ", encrypted" } else { "" }
        ),
    );
    r.field("Negotiated", "encryption flags", format!("{:#x}", n.encryption_enabled));
    r.field(
        "Negotiated",
        "host feature flags",
        format!("{:#x}", n.host_feature_flags),
    );
    r.field(
        "Negotiated",
        "ports",
        format!(
            "video {} · audio {} · control {}",
            n.video_port, n.audio_port, n.control_port
        ),
    );
    r.field("Negotiated", "packet size", n.packet_size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_prepare_validation() {
        assert_eq!(hermes_platform("android"), "unknown");
        assert_eq!(hermes_platform("linux"), "linux");
        let limits = hestia_api::Limits {
            max_width: 7680,
            max_height: 4320,
            max_fps: 240,
            supported_fps: vec![30, 40, 45, 60, 90, 120],
            supported_codecs: vec![],
        };
        let mut s = StreamSettings::default();
        (s.width, s.height, s.fps) = (1920, 1080, 60);
        assert_eq!(check_prepare_limits(&limits, &s), Ok(()));
        s.fps = 144;
        assert!(check_prepare_limits(&limits, &s).unwrap_err().contains("144 fps"));
        s.fps = 60;
        s.width = 8000;
        assert!(check_prepare_limits(&limits, &s).is_err());
        assert_eq!(check_prepare_limits(&hestia_api::Limits::default(), &s), Ok(()));
    }
    use hestia_stream::{StreamConfig, VideoCodec, VideoFormat};

    fn session(url: &str, codec: CodecPreference) -> LaunchedSession {
        let mut request = LaunchRequest::new(7, 1280, 720, 120);
        request.audio = AudioConfig::Surround51;
        request.remote_input_key = [9; 16];
        request.remote_input_key_id = 42;
        LaunchedSession {
            host: HostRecord {
                address: "192.168.1.20".into(),
                ..Default::default()
            },
            request,
            response: LaunchResponse {
                session_url: url.into(),
                resumed: false,
            },
            server_info: ServerInfo {
                codec_mode_support: 0x0101,
                ..Default::default()
            },
            prepared: None,
            prepare_error: None,
            settings: StreamSettings {
                bitrate_kbps: 15_000,
                codec,
                ..Default::default()
            },
        }
    }

    #[test]
    fn launch_report_distinguishes_capability_probe_failure_from_a_missing_endpoint() {
        let unsupported = hestia_api::ApiError::Host {
            status: 404,
            code: String::new(),
            message: String::new(),
        };
        assert!(session_prepare_capabilities(Err(unsupported)).unwrap().is_none());
        let mut launch = session("rtspenc://192.168.1.20:48010", CodecPreference::Auto);
        let mut fallback = report::StreamReport::new("Desktop", 0);
        launch.add_to_report(&mut fallback);
        assert!(fallback.render().contains("not prepared (Hestia API unavailable)"));

        let timeout = hestia_api::ApiError::Transport(hestia_nvhttp::NvHttpError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "probe timed out",
        )));
        launch.response.resumed = true;
        launch.prepare_error = Some(session_prepare_capabilities(Err(timeout)).unwrap_err());
        let mut resumed = report::StreamReport::new("Desktop", 0);
        launch.add_to_report(&mut resumed);
        let rendered = resumed.render();
        assert!(rendered.contains("Hestia capabilities check failed: network error: probe timed out"));
        assert!(!rendered.contains("not a Hermes host"));
    }

    #[test]
    fn session_preparation_requires_compatible_capabilities() {
        let caps = Capabilities {
            ok: true,
            hestia_protocol: 2,
            min_client_protocol: 2,
            max_client_protocol: 3,
            ..Default::default()
        };
        let why = session_prepare_capabilities(Ok(caps.clone())).unwrap_err();
        assert!(why.contains("host protocol 2"));
        let compatible = Capabilities {
            min_client_protocol: 1,
            ..caps
        };
        assert!(session_prepare_capabilities(Ok(compatible)).unwrap().is_some());
    }

    const H264: VideoFormat = VideoFormat {
        codec: VideoCodec::H264,
        ten_bit: false,
        yuv444: false,
    };
    const HEVC: VideoFormat = VideoFormat {
        codec: VideoCodec::Hevc,
        ten_bit: false,
        yuv444: false,
    };
    const HEVC10: VideoFormat = VideoFormat {
        codec: VideoCodec::Hevc,
        ten_bit: true,
        yuv444: false,
    };

    #[test]
    fn stream_config_from_launch() {
        let s = session("rtspenc://192.168.1.20:48010", CodecPreference::Auto);
        let c = s.stream_config(&[HEVC10, HEVC, H264]).unwrap();
        assert!(c.session_url.encrypted);
        assert_eq!((c.width, c.height, c.fps, c.bitrate_kbps), (1280, 720, 120, 15_000));
        assert_eq!((c.audio_channels, c.audio_channel_mask), (6, 0x3f));
        assert_eq!((c.remote_input_key, c.remote_input_key_id), ([9; 16], 42));
        assert_eq!(c.server_codec_modes, 0x0101);
        assert!(!c.streaming_remotely);
        assert_eq!(c.packet_size, StreamConfig::LAN_PACKET_SIZE);
        // No HDR requested: 10-bit formats are not offered.
        assert_eq!(c.video_formats, vec![HEVC, H264]);
    }

    #[test]
    fn pyrowave_is_explicit_even_with_a_multi_format_sink() {
        let pyro = VideoFormat {
            codec: VideoCodec::PyroWave,
            ..H264
        };
        for (pref, expected) in [(CodecPreference::Auto, H264), (CodecPreference::PyroWave, pyro)] {
            assert_eq!(
                session("", pref).stream_config(&[pyro, H264]).unwrap().video_formats,
                [expected]
            );
        }
        assert!(session("", CodecPreference::PyroWave).stream_config(&[H264]).is_err());
    }

    #[test]
    fn host_records_saved_before_addresses_load() {
        let json = r#"{"id":"UUID","name":"cachy","address":"192.168.0.10","http_port":47989,"manual":true}"#;
        let record: HostRecord = serde_json::from_str(json).unwrap();
        assert!(record.addresses.is_empty());
        assert_eq!(record.known_addresses(), [HostAddress::new("192.168.0.10", 47989)]);
    }

    #[test]
    fn stream_config_preferences_and_fallbacks() {
        let s = session("rtsp://203.0.113.9:48010", CodecPreference::H264);
        let c = s.stream_config(&[HEVC, H264]).unwrap();
        assert_eq!(c.video_formats, vec![H264, HEVC]);
        assert!(c.streaming_remotely);
        assert_eq!(c.packet_size, StreamConfig::WAN_PACKET_SIZE);

        // Old hosts send no session URL: use the stored address.
        let c = session("", CodecPreference::Auto).stream_config(&[H264]).unwrap();
        assert_eq!(
            (c.session_url.host.as_str(), c.session_url.port),
            ("192.168.1.20", 48010)
        );

        assert!(session("", CodecPreference::Auto).stream_config(&[HEVC10]).is_err());
    }
}

#[cfg(test)]
mod host_records {
    use super::*;

    /// A fresh directory for one test's store.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hestia-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn host_records_saved_before_profiles_load() {
        // A profile missing a field (an older file) doesn't lose the host.
        let json = r#"{"hosts":[{"id":"UUID","name":"cachy","address":"192.168.0.10",
            "profile":"Couch","profiles":[{"name":"Couch","fps":120}]}]}"#;
        let stored: store::StoredHosts = serde_json::from_str(json).unwrap();
        let [record] = stored.hosts.as_slice() else {
            panic!("one host expected");
        };
        assert_eq!(record.profiles.len(), 1);
        assert_eq!(record.profiles[0].fps, 120);
        assert_eq!(record.profiles[0].width, StreamSettings::default().width);
        assert_eq!(record.display_name(), "cachy");
        assert_eq!(record.default_profile, None);
    }

    #[test]
    fn legacy_profiles_move_into_each_host_once() {
        let dir = test_dir("legacy-profiles");
        let store = Store::open(&dir).unwrap();
        let couch = profiles::StreamProfile::from_settings(
            "Couch",
            &StreamSettings {
                fps: 120,
                ..StreamSettings::default()
            },
        );
        std::fs::write(dir.join("profiles.json"), serde_json::to_vec(&[&couch]).unwrap()).unwrap();
        let own = profiles::StreamProfile::from_settings("Own", &StreamSettings::default());
        let mut hosts = vec![
            HostRecord {
                profile: Some("Couch".into()),
                ..host("A", "192.168.0.10")
            },
            HostRecord {
                profiles: vec![own.clone()],
                ..host("B", "192.168.0.11")
            },
        ];
        adopt_legacy_profiles(&store, &mut hosts);
        assert_eq!(hosts[0].profiles, [couch.clone()]);
        assert_eq!(hosts[0].profile.as_deref(), Some("Couch"));
        assert_eq!(hosts[1].profiles, [own]);
        assert_eq!(store.load_hosts().unwrap()[0].profiles, [couch]);
        assert!(!dir.join("profiles.json").exists());
        assert!(dir.join("profiles.legacy.json").exists());
        // Once: profiles a host deletes later don't come back.
        hosts[0].profiles.clear();
        adopt_legacy_profiles(&store, &mut hosts);
        assert!(hosts[0].profiles.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profiles_names_and_addresses_belong_to_their_host() {
        let dir = test_dir("host-profiles");
        Store::open(&dir)
            .unwrap()
            .save_hosts(&[host("A", "192.168.0.10"), host("B", "192.168.0.11")])
            .unwrap();
        let client = HestiaClient::open(&dir).unwrap();
        let tv = StreamSettings {
            width: 3840,
            height: 2160,
            ..StreamSettings::default()
        };
        assert_eq!(client.save_profile("A", " Couch  TV ", &tv).unwrap(), "Couch TV");
        client.set_host_profile("A", Some("couch tv")).unwrap();
        assert_eq!(client.host("A").unwrap().profile.as_deref(), Some("Couch TV"));
        assert!(client.set_host_profile("B", Some("Couch TV")).is_err());
        assert!(client.profiles("B").unwrap().is_empty());
        let (settings, profile) = client.settings_for_host("A");
        assert_eq!((settings.width, profile.as_deref()), (3840, Some("Couch TV")));
        assert_eq!(client.settings_for_host("B"), (client.settings(), None));

        // Saving the page goes to the profile in use, or the host's defaults.
        let fast = StreamSettings {
            fps: 120,
            ..StreamSettings::default()
        };
        client.set_host_settings("A", &fast).unwrap();
        client.set_host_settings("B", &fast).unwrap();
        assert_eq!(client.profiles("A").unwrap()[0].fps, 120);
        assert_eq!(client.settings_for_host("B").0.fps, 120);
        assert_eq!(client.settings().fps, StreamSettings::default().fps);

        client.delete_profile("A", "COUCH TV").unwrap();
        assert_eq!(client.host("A").unwrap().profile, None);

        client.edit_host_details("B", "  Desk ", "10.0.0.5:48000").unwrap();
        let b = client.host("B").unwrap();
        assert_eq!(b.display_name(), "Desk");
        assert_eq!(b.active_address(), HostAddress::new("10.0.0.5", 48000));
        assert!(b.knows("192.168.0.11"));
        // Its own name again: it follows what the host reports.
        client.edit_host_details("B", "cachy", "10.0.0.5:48000").unwrap();
        assert_eq!(client.host("B").unwrap().display_name, None);
        assert!(client.edit_host_details("B", &"x".repeat(81), "10.0.0.5").is_err());
        assert!(matches!(
            client.edit_host_details("C", "", "10.0.0.6"),
            Err(CoreError::UnknownHost(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn host(id: &str, address: &str) -> HostRecord {
        HostRecord {
            id: id.into(),
            name: "cachy".into(),
            address: address.into(),
            addresses: vec![HostAddress::new(address, DEFAULT_HTTP_PORT)],
            ..Default::default()
        }
    }

    #[test]
    fn failed_probe_does_not_change_an_edited_or_deleted_host() {
        let dir = test_dir("stale-host-probe");
        let store = Store::open(&dir).unwrap();
        let new_address = "192.168.0.11";
        let mut old = host("A", "192.168.0.10");
        old.remember_address(HostAddress::new(new_address, DEFAULT_HTTP_PORT));
        store.save_hosts(std::slice::from_ref(&old)).unwrap();
        let client = HestiaClient::open(&dir).unwrap();
        let probe = Probe {
            answer: None,
            taken: vec![new_address.into()],
            error: Some("host did not answer".into()),
        };
        client.edit_host_details("A", "Desk", new_address).unwrap();
        let edited = client.host("A").unwrap();

        // The stale result would remove the user's new address and fall
        // back to the old one if it were applied.
        assert!(matches!(client.save_probe(&old, &probe), Err(CoreError::Invalid(_))));
        assert_eq!(client.host("A").unwrap(), edited);
        assert_eq!(store.load_hosts().unwrap(), std::slice::from_ref(&edited));

        client.forget("A").unwrap();
        assert!(matches!(
            client.save_probe(&edited, &probe),
            Err(CoreError::UnknownHost(_))
        ));
        assert!(client.hosts().is_empty());
        assert!(store.load_hosts().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What `refresh` stores once a new address answered as a known host.
    fn answered(pending: &HostRecord, id: &str) -> HostRecord {
        let mut answered = pending.clone();
        answered.set_active(pending.active_address());
        answered.id = id.into();
        answered
    }

    #[test]
    fn tailscale_address_merges_into_the_paired_host() {
        let mut lan = host("UUID", "192.168.0.10");
        lan.server_cert_pem = Some("CERT".into());
        lan.profile = Some("Couch".into());
        let pending = host("pending:100.101.102.103:47989", "100.101.102.103");
        let mut hosts = vec![lan, host("OTHER", "192.168.0.20"), pending.clone()];

        let merged = store_merged(&mut hosts, &pending.id, answered(&pending, "UUID"));

        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0], merged, "keeps the older record's place");
        assert_eq!(hosts[1].id, "OTHER");
        assert!(merged.is_paired());
        assert_eq!(merged.profile.as_deref(), Some("Couch"));
        assert_eq!(merged.address, "100.101.102.103");
        assert!(merged.knows("192.168.0.10"));
        assert!(merged.knows("100.101.102.103"));
    }

    #[test]
    fn unknown_host_keeps_its_own_record() {
        let pending = host("pending:192.168.0.30:47989", "192.168.0.30");
        let mut hosts = vec![host("OTHER", "192.168.0.20"), pending.clone()];
        let stored = store_merged(&mut hosts, &pending.id, answered(&pending, "NEW"));
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[1], stored);
        assert_eq!(stored.id, "NEW");
        assert!(!stored.is_paired());

        let mut hosts = vec![host("OTHER", "192.168.0.20")];
        store_merged(&mut hosts, "gone", host("NEW", "192.168.0.30"));
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn pairing_again_keeps_the_new_certificate() {
        let mut old = host("UUID", "192.168.0.10");
        old.server_cert_pem = Some("OLD".into());
        let mut fresh = host("pending:x", "100.101.102.103");
        fresh.server_cert_pem = Some("NEW".into());
        let mut hosts = vec![old, fresh.clone()];
        fresh.id = "UUID".into();
        let merged = store_merged(&mut hosts, "pending:x", fresh);
        assert_eq!(hosts.len(), 1);
        assert_eq!(merged.server_cert_pem.as_deref(), Some("NEW"));
        assert!(merged.knows("192.168.0.10"));
    }

    #[test]
    fn new_lease_and_reused_address() {
        let mut h = host("UUID", "192.168.0.10");
        assert!(h.remember_address(HostAddress::new("192.168.0.11", DEFAULT_HTTP_PORT)));
        assert!(!h.remember_address(HostAddress::new("192.168.0.11", DEFAULT_HTTP_PORT)));
        assert_eq!(h.address, "192.168.0.10", "learning an address doesn't switch to it");

        // The old lease went to another PC we know.
        h.forget_address("192.168.0.10");
        assert_eq!(h.address, "192.168.0.11");
        assert!(!h.knows("192.168.0.10"));
        // The last address stays.
        h.forget_address("192.168.0.11");
        assert_eq!(h.address, "192.168.0.11");
    }

    #[test]
    fn records_from_before_addresses() {
        let legacy = HostRecord {
            id: "UUID".into(),
            address: "192.168.0.10".into(),
            http_port: 48000,
            ..Default::default()
        };
        assert_eq!(legacy.known_addresses(), [HostAddress::new("192.168.0.10", 48000)]);
        assert!(legacy.knows("192.168.0.10"));

        let mut legacy = legacy;
        assert!(legacy.remember_address(HostAddress::new("100.101.102.103", DEFAULT_HTTP_PORT)));
        assert_eq!(
            legacy.addresses,
            [
                HostAddress::new("192.168.0.10", 48000),
                HostAddress::new("100.101.102.103", DEFAULT_HTTP_PORT)
            ]
        );
        // The port of the address in use follows what we learn about it.
        legacy.remember_address(HostAddress::new("192.168.0.10", DEFAULT_HTTP_PORT));
        assert_eq!(legacy.http_port, DEFAULT_HTTP_PORT);
    }
}
