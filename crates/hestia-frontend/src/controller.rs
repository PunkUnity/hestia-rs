//! The app's logic between the screen and `hestia-core`, without a toolkit.
//!
//! The public methods are the user's intents (open a host, launch an app,
//! a key in the stream…); a UI shell calls them from its callbacks. Results
//! reach the screen through the [`UiHandle`] port as closures over a
//! [`View`].
//!
//! Rules: intents run on the UI thread and must never block, so every core
//! call goes to a worker thread; results come back through
//! [`Controller::ui`]. Shared state sits behind one mutex that is never held
//! across network calls.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use hestia_core::diagnostics_overlay::DiagnosticsOverlay;
use hestia_core::nvhttp::{AppInfo, PairingOutcome, PairingPin};
use hestia_core::shortcuts::{ShortcutAction, ShortcutBinding, Shortcuts};
use hestia_core::{DeviceInfo, HestiaClient, HostSnapshot, Preset, presets};
use log::{info, warn};

use crate::presentation::GpuWindow;
use crate::ui::{Art, Screen, UiHandle, View};
use crate::{platform, view};

mod clipboard;
mod profiles;
mod report;
mod streaming;
mod tools;

#[derive(Default)]
struct State {
    snapshots: HashMap<String, HostSnapshot>,
    checking: HashSet<String>,
    current_host: Option<String>,
    apps: Vec<AppInfo>,
    /// Host being paired and a generation counter that invalidates results
    /// of abandoned attempts.
    pairing_host: Option<String>,
    pair_generation: u64,
    /// Hermes server commands on the tools screen, and the one waiting for
    /// its confirmation tap.
    tools_commands: Vec<hestia_core::api::ServerCommand>,
    tools_armed: Option<u32>,
    /// The launch in progress (sequence number, host id); cancelling clears
    /// it so the launch thread knows to back out.
    launching: Option<(u64, String)>,
    launch_seq: u64,
}

pub struct Controller {
    /// The UI shell's side of the port.
    handle: Arc<dyn UiHandle>,
    core: OnceLock<Arc<HestiaClient>>,
    state: Mutex<State>,
    stream: Mutex<Option<streaming::ActiveStream>>,
    stream_generation: AtomicU64,
    /// The window's GPU video renderer (Android zero-copy, desktop colour
    /// conversion) and the stream it draws.
    gpu_window: Arc<GpuWindow>,
    /// What a finger does in the stream (`TouchMode` index; persisted).
    touch_mode: AtomicU8,
    /// The platform hands every finger over itself
    /// ([`Controller::stream_raw_pointer`]), below the toolkit.
    raw_touch: AtomicBool,
    /// The report screen's content.
    last_report: Mutex<Option<report::LastReport>>,
}

/// The controller is shared by the shell's callbacks and its worker threads.
pub type Shared = Arc<Controller>;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        warn!("failed to spawn {name}: {e}");
    }
}

/// The shortcut editor's labels, in [`ShortcutAction::ALL`] order.
fn shortcut_labels(shortcuts: Shortcuts) -> [String; 3] {
    ShortcutAction::ALL.map(|action| shortcuts.get(action).label())
}

impl Controller {
    /// `handle` reaches the shell's window; `gpu_window` is the window's GPU
    /// video renderer (the shell installs it, see `presentation`).
    pub fn new(handle: Arc<dyn UiHandle>, gpu_window: Arc<GpuWindow>) -> Shared {
        Arc::new(Self {
            handle,
            core: OnceLock::new(),
            state: Mutex::new(State::default()),
            stream: Mutex::new(None),
            stream_generation: AtomicU64::new(0),
            gpu_window,
            touch_mode: AtomicU8::new(0),
            raw_touch: AtomicBool::new(false),
            last_report: Mutex::new(None),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn core(&self) -> Option<Arc<HestiaClient>> {
        self.core.get().cloned()
    }

    /// Runs `f` on the UI thread, with the window's view.
    fn ui(&self, f: impl FnOnce(&dyn View) + Send + 'static) {
        self.handle.run(Box::new(f));
    }

    /// Like [`Controller::ui`], once `delay` has passed.
    fn ui_after(&self, delay: Duration, f: impl FnOnce(&dyn View) + Send + 'static) {
        self.handle.run_after(delay, Box::new(f));
    }

    /// The UI port, for the video presenter.
    pub(crate) fn handle(&self) -> Arc<dyn UiHandle> {
        self.handle.clone()
    }

    fn toast(self: &Shared, message: impl Into<String>) {
        let message: String = message.into();
        let shown = message.clone();
        self.ui(move |ui| ui.set_toast(shown));
        self.ui_after(Duration::from_secs(4), move |ui| {
            if ui.toast() == message {
                ui.set_toast(String::new());
            }
        });
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    /// Back from the apps page to the hosts.
    pub fn back_to_hosts(self: &Shared) {
        self.state().current_host = None;
        self.ui(|ui| ui.set_screen(Screen::Hosts));
        self.push_hosts();
    }

    /// Back from the launch, stream report or tools pages to the apps.
    pub fn back_to_apps(self: &Shared) {
        // While starting, "back" means cancel.
        if self.cancel_launch() {
            return;
        }
        // The report screen can be opened from Settings with no host open.
        let screen = if self.state().current_host.is_some() {
            Screen::Apps
        } else {
            Screen::Hosts
        };
        self.ui(move |ui| ui.set_screen(screen));
        self.refresh_current_running();
    }

    /// Load the settings for the selected host, or device defaults on the
    /// hosts screen. Hardware-specific controls depend on current capabilities.
    pub fn open_settings(self: &Shared) {
        if let Some(core) = self.core() {
            let selected = self.state().current_host.clone();
            let (settings, host_name) = match selected {
                Some(ref id) => {
                    let (settings, _) = core.settings_for_host(id);
                    let name = core
                        .host(id)
                        .map(|host| host.display_name().to_owned())
                        .unwrap_or_default();
                    (settings, name)
                }
                None => (core.settings(), String::new()),
            };
            let shortcuts = core.settings().shortcuts;
            self.ui(move |ui| {
                ui.set_settings_host_name(host_name);
                ui.set_physical_keyboard(platform::has_physical_keyboard());
                ui.set_shortcut_labels(shortcut_labels(shortcuts));
                view::load_settings_into(ui, &settings, platform::native_display());
            });
            self.push_profiles();
        }
    }

    // ------------------------------------------------------------------
    // Startup
    // ------------------------------------------------------------------

    /// Opens the core (identity, hosts, settings) from `data_dir` on a worker
    /// thread, fills the screens and starts host discovery. Call once, after
    /// the shell is ready to run [`UiHandle`] tasks.
    pub fn start(self: &Shared, data_dir: PathBuf) {
        // Keyboard text and game controllers reach the stream from the
        // platform layer, not from the toolkit.
        let c = Arc::downgrade(self);
        platform::set_ime_sink(move |event| {
            if let Some(c) = c.upgrade() {
                c.stream_ime(event);
            }
        });
        let c = Arc::downgrade(self);
        platform::set_gamepad_sink(move |state| {
            if let Some(c) = c.upgrade() {
                c.stream_gamepad(state);
            }
        });
        // Physical keys by position, and every finger and pen, below the
        // toolkit (Android; the desktop shell calls the intents itself).
        let c = Arc::downgrade(self);
        platform::set_key_sink(move |code, pressed, repeat| {
            c.upgrade()
                .is_some_and(|c| c.stream_physical_key(code, pressed, repeat))
        });
        let c = Arc::downgrade(self);
        platform::set_pointer_sink(move |pointer| c.upgrade().is_some_and(|c| c.stream_raw_pointer(pointer)));
        self.ui(|ui| {
            ui.set_stream_soft_keyboard(platform::has_soft_keyboard());
            ui.set_stream_pointer_capture_available(platform::has_pointer_capture());
        });
        let c = self.clone();
        spawn("hestia-init", move || {
            c.ui(|ui| ui.set_starting_text("Loading this device's identity…".into()));
            let core = match HestiaClient::open(&data_dir) {
                Ok(core) => Arc::new(core),
                Err(e) => {
                    let text = format!("Could not start: {e}\n({})", data_dir.display());
                    c.ui(move |ui| ui.set_starting_text(text.into()));
                    return;
                }
            };
            let _ = c.core.set(core.clone());
            // Keep the log on disk too, so a killed app leaves it behind.
            if let Err(e) = hestia_core::logcapture::persist(&platform::reports_dir(core.data_dir())) {
                warn!("could not keep the log on disk: {e}");
            }
            info!("{} starting", view::version_text());
            if let Some(device) = platform::device_summary() {
                info!("device: {device}");
            }
            let settings = core.settings();
            c.touch_mode.store(settings.touch().index() as u8, Ordering::Relaxed);
            let client_id = core.identity().unique_id().to_owned();
            let dir = core.data_dir().display().to_string();
            c.ui(move |ui| {
                view::load_settings_into(ui, &settings, platform::native_display());
                ui.set_stream_touch_mode(settings.touch().index());
                ui.set_version_text(view::version_text().into());
                ui.set_client_id(client_id.into());
                ui.set_data_dir(dir.into());
                ui.set_starting(false);
            });
            c.refresh_all(false);
            c.start_discovery();
        });
    }

    fn start_discovery(self: &Shared) {
        let controller = Arc::downgrade(self);
        spawn("hestia-discovery", move || {
            let (browser, rx) = match hestia_core::discovery::Browser::start() {
                Ok(v) => v,
                Err(e) => {
                    warn!("mDNS discovery unavailable: {e}");
                    return;
                }
            };
            if let Some(c) = controller.upgrade() {
                c.set_scanning_for(Duration::from_secs(5));
            }
            // Android can destroy and reopen the Activity in the same
            // process when PiP is dismissed. Stop the old window's browser.
            while controller.strong_count() > 0 {
                let found = match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(found) => found,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                let Some(c) = controller.upgrade() else { break };
                let Some(core) = c.core() else { break };
                match core.note_discovered(&found) {
                    Ok(note) => {
                        info!("discovered {} at {:?}", found.name, found.addresses);
                        // Check again when the host is new, showed up at a new
                        // address (a new DHCP lease, back on its LAN) or was
                        // offline: the record may now reach it another way.
                        let check = {
                            let st = c.state();
                            !st.checking.contains(&note.id)
                                && (note.learned_address || st.snapshots.get(&note.id).is_none_or(|s| !s.online))
                        };
                        if check {
                            c.refresh_host(note.id);
                        } else {
                            c.push_hosts();
                        }
                    }
                    Err(e) => warn!("ignoring discovered host {}: {e}", found.name),
                }
            }
            drop(browser);
        });
    }

    fn set_scanning_for(self: &Shared, duration: Duration) {
        self.ui(|ui| ui.set_scanning(true));
        let c = self.clone();
        spawn("hestia-scan-timer", move || {
            std::thread::sleep(duration);
            c.ui(|ui| ui.set_scanning(false));
        });
    }

    // ------------------------------------------------------------------
    // Hosts
    // ------------------------------------------------------------------

    fn push_hosts(self: &Shared) {
        let Some(core) = self.core() else { return };
        let cards: Vec<_> = {
            let st = self.state();
            core.hosts()
                .iter()
                .map(|r| view::host_card(r, st.snapshots.get(&r.id), st.checking.contains(&r.id)))
                .collect()
        };
        self.ui(move |ui| ui.set_hosts(cards));
    }

    /// Checks every known host again (`scan_indicator`: show the scanning
    /// spinner for a moment, as for a pull to refresh).
    pub fn refresh_all(self: &Shared, scan_indicator: bool) {
        let Some(core) = self.core() else { return };
        if scan_indicator {
            self.set_scanning_for(Duration::from_secs(3));
        }
        for host in core.hosts() {
            self.refresh_host(host.id);
        }
        self.push_hosts();
    }

    fn refresh_host(self: &Shared, id: String) {
        self.state().checking.insert(id.clone());
        self.push_hosts();
        let c = self.clone();
        spawn("hestia-refresh", move || {
            let Some(core) = c.core() else { return };
            let result = core.refresh(&id);
            {
                let mut st = c.state();
                st.checking.remove(&id);
                if let Ok(snapshot) = &result {
                    st.snapshots.remove(&id);
                    if st.current_host.as_deref() == Some(id.as_str()) {
                        st.current_host = Some(snapshot.record.id.clone());
                    }
                    st.snapshots.insert(snapshot.record.id.clone(), snapshot.clone());
                }
            }
            if let Err(e) = result {
                warn!("refresh of {id} failed: {e}");
            }
            c.push_hosts();
        });
    }

    /// Adds a host by IP address or name.
    pub fn add_host(self: &Shared, address: String) {
        let address = address.trim().to_owned();
        if address.is_empty() {
            return;
        }
        self.ui(|ui| {
            ui.set_add_busy(true);
            ui.set_add_error(String::new());
        });
        let c = self.clone();
        spawn("hestia-add", move || {
            let Some(core) = c.core() else { return };
            match core.add_manual(&address) {
                Ok(snapshot) => {
                    let name = snapshot.record.display_name().to_owned();
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
                    c.ui(|ui| {
                        ui.set_add_busy(false);
                        ui.set_add_visible(false);
                        ui.set_add_address(String::new());
                    });
                    c.toast(format!("Added {name}"));
                }
                Err(e) => {
                    let text = e.to_string();
                    c.ui(move |ui| {
                        ui.set_add_busy(false);
                        ui.set_add_error(text.into());
                    });
                }
            }
            c.push_hosts();
        });
    }

    /// A host card tapped: its apps, pairing, or Wake-on-LAN.
    pub fn open_host(self: &Shared, id: String) {
        let Some(core) = self.core() else { return };
        let Ok(record) = core.host(&id) else { return };
        let snapshot = self.state().snapshots.get(&id).cloned();
        let name = record.display_name().to_owned();
        match snapshot {
            Some(s) if s.online && s.paired => self.show_apps(s),
            Some(s) if s.online => self.start_pairing(id, name),
            Some(_) if record.mac.is_some() => self.wake_host(id, name),
            Some(_) => {
                self.toast(format!(
                    "{name} is offline (Wake-on-LAN needs its MAC address, learned once it's been online)"
                ));
                self.refresh_host(id);
            }
            None => self.toast(format!("Still contacting {name}…")),
        }
    }

    /// Wake-on-LAN, then check every few seconds for a minute.
    fn wake_host(self: &Shared, id: String, name: String) {
        let c = self.clone();
        spawn("hestia-wake", move || {
            let Some(core) = c.core() else { return };
            match core.wake(&id) {
                Ok(_) => c.toast(format!("Waking {name}… this can take a minute")),
                Err(e) => return c.toast(format!("Could not wake {name}: {e}")),
            }
            for _ in 0..12 {
                std::thread::sleep(Duration::from_secs(5));
                if let Ok(snapshot) = core.refresh(&id) {
                    let online = snapshot.online;
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
                    c.push_hosts();
                    if online {
                        c.toast(format!("{name} is awake"));
                        return;
                    }
                }
            }
            c.toast(format!("{name} didn't answer; check that Wake-on-LAN is enabled on it"));
        });
    }

    /// Host page → Edit: the dialog for the open host's name on this
    /// device and the address it is reached at.
    pub fn edit_current_host(self: &Shared) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let Ok(host) = core.host(&id) else { return };
        let name = host.display_name().to_owned();
        let address = host.active_address().to_string();
        self.ui(move |ui| {
            ui.set_host_edit_name(name);
            ui.set_host_edit_address(address);
            ui.set_host_edit_error(String::new());
            ui.set_host_edit_visible(true);
        });
    }

    /// The edit dialog's Save. The pairing stays: it belongs to the host,
    /// not to an address.
    pub fn save_current_host(self: &Shared, name: String, address: String) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        if let Err(e) = core.edit_host_details(&id, &name, &address) {
            let message = e.to_string();
            self.ui(move |ui| ui.set_host_edit_error(message));
            return;
        }
        let header = core.host(&id).ok().map(|record| {
            let mut st = self.state();
            match st.snapshots.get_mut(&id) {
                Some(snapshot) => {
                    snapshot.record = record;
                    (snapshot.record.display_name().to_owned(), view::host_detail(snapshot))
                }
                None => (record.display_name().to_owned(), record.address.clone()),
            }
        });
        self.ui(move |ui| {
            if let Some((name, detail)) = header {
                ui.set_host_name(name);
                ui.set_host_detail(detail);
            }
            ui.set_host_edit_visible(false);
            ui.set_host_edit_error(String::new());
        });
        self.push_hosts();
        // Whether it answers at the new address.
        self.refresh_host(id);
        self.toast("Host updated");
    }

    /// Forgets the open host (its pairing too).
    pub fn forget_current(self: &Shared) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.take() else {
            return;
        };
        let name = core.host(&id).map(|h| h.display_name().to_owned()).unwrap_or_default();
        if let Err(e) = core.forget(&id) {
            self.toast(format!("Could not forget host: {e}"));
            return;
        }
        self.state().snapshots.remove(&id);
        self.ui(|ui| ui.set_screen(Screen::Hosts));
        self.push_hosts();
        self.toast(format!("Forgot {name}. Remove this device in the host's web UI too."));
    }

    // ------------------------------------------------------------------
    // Pairing
    // ------------------------------------------------------------------

    fn start_pairing(self: &Shared, id: String, name: String) {
        let generation = {
            let mut st = self.state();
            st.pair_generation += 1;
            st.pairing_host = Some(id.clone());
            st.pair_generation
        };
        let pin = PairingPin::random();
        let pin_text = pin.as_str().to_owned();
        self.ui(move |ui| {
            ui.set_pair_host_name(name.into());
            ui.set_pair_pin(pin_text);
            ui.set_pair_status("Waiting for the PIN to be entered on the host…".into());
            ui.set_pair_busy(true);
            ui.set_pair_failed(false);
            ui.set_pair_otp_mode(false);
            ui.set_pair_visible(true);
        });
        self.run_pairing(id, generation, pin, None);
    }

    fn run_pairing(self: &Shared, id: String, generation: u64, pin: PairingPin, passphrase: Option<String>) {
        let c = self.clone();
        spawn("hestia-pair", move || {
            let Some(core) = c.core() else { return };
            let result = core.pair(&id, &pin, passphrase.as_deref());
            if c.state().pair_generation != generation {
                return; // abandoned attempt
            }
            let failure = match result {
                Ok((PairingOutcome::Paired { .. }, new_id)) => {
                    c.state().pairing_host = None;
                    c.ui(|ui| ui.set_pair_visible(false));
                    c.toast("Paired");
                    c.after_pairing(new_id);
                    return;
                }
                Ok((PairingOutcome::WrongPin, _)) => "That PIN didn't match. Try again with a new one.".to_owned(),
                Ok((PairingOutcome::AlreadyInProgress, _)) => {
                    "The host is busy with another pairing attempt. Try again in a moment.".to_owned()
                }
                Ok((PairingOutcome::Failed(why), _)) => why,
                Err(e) => {
                    let text = e.to_string();
                    match crate::hostinfo::network_hint(&text) {
                        Some(hint) => format!("{text}\n{hint}"),
                        None => text,
                    }
                }
            };
            c.ui(move |ui| {
                ui.set_pair_busy(false);
                ui.set_pair_failed(true);
                ui.set_pair_status(failure.into());
            });
        });
    }

    fn after_pairing(self: &Shared, id: String) {
        let c = self.clone();
        spawn("hestia-post-pair", move || {
            let Some(core) = c.core() else { return };
            match core.refresh(&id) {
                Ok(snapshot) => {
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot.clone());
                    c.push_hosts();
                    if snapshot.paired {
                        c.show_apps(snapshot);
                    }
                }
                Err(e) => warn!("refresh after pairing failed: {e}"),
            }
        });
    }

    /// Pairing failed: try again with a new PIN.
    pub fn restart_pairing(self: &Shared) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().pairing_host.clone() else {
            return;
        };
        let name = core.host(&id).map(|h| h.display_name().to_owned()).unwrap_or_default();
        self.start_pairing(id, name);
    }

    /// Pair with a PIN and passphrase shown by the host (Hermes, Apollo).
    pub fn switch_to_otp(self: &Shared) {
        let Some(id) = self.state().pairing_host.clone() else {
            return;
        };
        self.state().pair_generation += 1;
        self.ui(|ui| {
            ui.set_pair_otp_mode(true);
            ui.set_pair_busy(false);
            ui.set_pair_failed(false);
            ui.set_pair_status(String::new());
        });
        // Drop the pending client-PIN attempt on the host.
        let c = self.clone();
        spawn("hestia-pair-cancel", move || {
            if let Some(core) = c.core() {
                core.cancel_pairing(&id);
            }
        });
    }

    /// The PIN and passphrase the host showed.
    pub fn submit_otp(self: &Shared, pin: String, passphrase: String) {
        let Some(pin) = PairingPin::parse(&pin) else {
            self.ui(|ui| {
                ui.set_pair_failed(true);
                ui.set_pair_status("The PIN must be at least 4 digits.".into());
            });
            return;
        };
        let (id, generation) = {
            let mut st = self.state();
            st.pair_generation += 1;
            (st.pairing_host.clone(), st.pair_generation)
        };
        let Some(id) = id else { return };
        self.ui(|ui| {
            ui.set_pair_busy(true);
            ui.set_pair_failed(false);
            ui.set_pair_status("Pairing…".into());
        });
        self.run_pairing(id, generation, pin, Some(passphrase.trim().to_owned()));
    }

    pub fn cancel_pairing(self: &Shared) {
        let id = {
            let mut st = self.state();
            st.pair_generation += 1;
            st.pairing_host.take()
        };
        self.ui(|ui| {
            ui.set_pair_visible(false);
            ui.set_otp_pin(String::new());
            ui.set_otp_passphrase(String::new());
        });
        if let Some(id) = id {
            let c = self.clone();
            spawn("hestia-pair-cancel", move || {
                if let Some(core) = c.core() {
                    core.cancel_pairing(&id);
                }
            });
        }
    }

    // ------------------------------------------------------------------
    // Apps
    // ------------------------------------------------------------------

    fn show_apps(self: &Shared, snapshot: HostSnapshot) {
        let id = snapshot.record.id.clone();
        self.state().current_host = Some(id.clone());
        let name = snapshot.record.display_name().to_owned();
        let detail = view::host_detail(&snapshot);
        let chips = view::host_chips(&snapshot);
        let running = snapshot.info.as_ref().is_some_and(|i| i.is_busy());
        let hermes = snapshot.capabilities.is_some();
        self.ui(move |ui| {
            ui.set_host_hermes(hermes);
            ui.set_host_name(name);
            ui.set_host_detail(detail);
            ui.set_host_chips(chips);
            ui.set_apps(Vec::new());
            ui.set_apps_loading(true);
            ui.set_app_running(running);
            ui.set_screen(Screen::Apps);
        });
        self.push_profiles();

        let running_app = snapshot.info.as_ref().map_or(0, |i| i.current_game);
        let c = self.clone();
        spawn("hestia-apps", move || {
            let Some(core) = c.core() else { return };
            let apps = match core.apps(&id) {
                Ok(apps) => apps,
                Err(e) => {
                    c.ui(|ui| ui.set_apps_loading(false));
                    c.toast(format!("Could not load apps: {e}"));
                    return;
                }
            };
            c.state().apps = apps.clone();
            let cards: Vec<_> = apps.iter().map(|a| view::app_card(a, running_app)).collect();
            c.ui(move |ui| {
                ui.set_apps(cards);
                ui.set_apps_loading(false);
            });

            for (index, app) in apps.iter().enumerate() {
                if c.state().current_host.as_deref() != Some(id.as_str()) {
                    return; // user left the page
                }
                let Ok(bytes) = core.box_art(&id, app.id) else { continue };
                let Some((raw, w, h)) = view::decode_art(&bytes) else {
                    continue;
                };
                let app_id = app.id as i32;
                let art = Art {
                    rgba: raw.into(),
                    width: w,
                    height: h,
                };
                c.ui(move |ui| ui.set_app_art(index, app_id, art));
            }
        });
    }

    /// Re-reads the host's running app and updates the app grid.
    fn refresh_current_running(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let c = self.clone();
        spawn("hestia-running", move || {
            let Some(core) = c.core() else { return };
            let Ok(snapshot) = core.refresh(&id) else { return };
            let running_app = snapshot.info.as_ref().map_or(0, |i| i.current_game);
            c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
            c.ui(move |ui| {
                ui.set_app_running(running_app != 0);
                ui.set_running_app(running_app);
            });
            c.push_hosts();
        });
    }

    /// Starts (or resumes) an app on the open host and streams it.
    pub fn launch(self: &Shared, app_id: u32) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let (title, host_name, address, other_running) = {
            let st = self.state();
            let title = st
                .apps
                .iter()
                .find(|a| a.id == app_id)
                .map(|a| a.title.clone())
                .unwrap_or_default();
            let snapshot = st.snapshots.get(&id);
            let current = snapshot.and_then(|s| s.info.as_ref()).map_or(0, |i| i.current_game);
            let host_name = snapshot.map(|s| s.record.display_name().to_owned()).unwrap_or_default();
            let address = snapshot.map(|s| s.record.address.clone()).unwrap_or_default();
            (title, host_name, address, current != 0 && current != app_id)
        };
        if other_running {
            self.toast("Another app is running on this host. Quit it first.");
            return;
        }

        let seq = {
            let mut st = self.state();
            if st.launching.is_some() {
                return;
            }
            st.launch_seq += 1;
            st.launching = Some((st.launch_seq, id.clone()));
            st.launch_seq
        };
        let c = self.clone();
        self.ui(move |ui| {
            ui.set_session_title(title.clone());
            ui.set_session_subtitle(host_name.clone());
            ui.set_session_lines(lines(&["Starting on the host…"]));
            ui.set_session_busy(true);
            ui.set_screen(Screen::Session);

            // This display, landscape (streams are), else the window.
            let mut device = DeviceInfo::default();
            if let Some((w, h, hz)) = platform::native_display() {
                (device.display_width, device.display_height, device.refresh_rate) = (w, h, hz);
            } else {
                let (width, height) = ui.window_size();
                if width > 0 && height > 0 {
                    device.display_width = width.max(height);
                    device.display_height = width.min(height);
                }
            }
            spawn("hestia-launch", move || {
                let Some(core) = c.core() else { return };
                let session_report = report::begin(&title, &host_name, &device);
                let failure: String;
                // The host's stream profile, if it has one, over the settings.
                let (mut settings, profile) = core.settings_for_host(&id);
                // Apply this build's supported choices to imported profiles as well.
                settings.codec = view::client_codec(settings.codec);
                #[cfg(target_os = "android")]
                if settings.metered_bitrate_kbps != 0 {
                    if let Ok(host) = core.host(&id)
                        && let Ok(ip) = host.address.parse::<std::net::IpAddr>()
                    {
                        platform::route_for(&std::net::SocketAddr::new(ip, host.http_port));
                    }
                    let metered = platform::stream_network_metered();
                    let mut report = report::lock(&session_report);
                    report.field("Settings", "normal bitrate", format!("{} kbps", settings.bitrate_kbps));
                    report.field(
                        "Settings",
                        "metered bitrate ceiling",
                        format!("{} kbps", settings.metered_bitrate_kbps),
                    );
                    report.field(
                        "Settings",
                        "metered network",
                        metered.map_or("unknown", |v| if v { "yes" } else { "no" }),
                    );
                    settings.bitrate_kbps = settings.bitrate_for_network(metered == Some(true));
                }
                if let Some(name) = profile {
                    info!("launching with stream profile “{name}”");
                    report::lock(&session_report).field("Settings", "profile", name);
                }
                let decoder = crate::video_backend::StreamVideo::select(&settings, &c.gpu_window);
                let launched = decoder.and_then(|decoder| {
                    if !c.launch_current(seq) {
                        return Err("cancelled".into());
                    }
                    core.launch_with(&id, app_id, &device, settings.clone())
                        .map(|session| (core.stream_host(&session), session, decoder))
                        .map_err(|error| error.to_string())
                });
                if !c.launch_current(seq) {
                    // Release the preparation before another attempt can start.
                    drop(launched);
                    c.finish_launch(seq);
                    c.finish_report(session_report, "cancelled while starting", Some(id.clone()));
                    return;
                }
                let text: Vec<String> = match launched {
                    Ok((host, session, decoder)) => {
                        report::launched(&session_report, &session);
                        if let Some(why) = &session.prepare_error {
                            c.toast(format!(
                                "Hermes declined to prepare this session ({why}); streaming in standard mode"
                            ));
                        }
                        c.ui(|ui| ui.set_session_lines(lines(&["Connecting to the stream…"])));
                        match c.start_stream(id.clone(), &session, host, decoder, session_report.clone(), seq) {
                            Ok(()) => {
                                c.finish_launch(seq);
                                c.ui(|ui| ui.set_session_busy(false));
                                c.refresh_current_running();
                                return;
                            }
                            Err(_) if !c.launch_current(seq) => {
                                c.finish_launch(seq);
                                c.finish_report(session_report, "cancelled while connecting", Some(id.clone()));
                                return;
                            }
                            Err(e) => {
                                failure = format!("the stream could not start: {e}");
                                let mut out = vec![format!(
                                    "{} on {}, but the stream could not start: {e}",
                                    if session.response.resumed { "Resumed" } else { "Started" },
                                    session.host.name
                                )];
                                if let Some(p) = &session.prepared {
                                    let vd = &p.virtual_display;
                                    out.push(format!(
                                        "Hermes display: {} via {}, {}×{} @ {}{}",
                                        vd.name,
                                        vd.backend,
                                        vd.width,
                                        vd.height,
                                        vd.fps,
                                        if vd.hdr { ", HDR" } else { "" }
                                    ));
                                    out.extend(p.warnings.iter().map(|w| format!("Warning: {w}")));
                                }
                                out.push("The app keeps running on the host until you quit it.".into());
                                out
                            }
                        }
                    }
                    Err(e) => {
                        report::settings(&session_report, &settings);
                        session_report
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .field("Host", "address", &address);
                        failure = format!("could not start the app: {e}");
                        vec![format!("Could not start the app: {e}")]
                    }
                };
                c.finish_launch(seq);
                let show_report = c.finish_report(session_report, &failure, Some(id.clone()));
                c.ui(move |ui| {
                    ui.set_session_lines(text);
                    ui.set_session_busy(false);
                    if show_report && ui.screen() == Screen::Session {
                        ui.set_screen(Screen::Report);
                    }
                });
                c.refresh_current_running();
            });
        });
    }

    fn launch_current(&self, seq: u64) -> bool {
        let state = self.state();
        state.launch_seq == seq && state.launching.as_ref().is_some_and(|(s, _)| *s == seq)
    }

    fn finish_launch(&self, seq: u64) {
        let mut st = self.state();
        if st.launching.as_ref().is_some_and(|(s, _)| *s == seq) {
            st.launching = None;
        }
    }

    /// Cancels the launch in progress, if any: back to the apps right away;
    /// the launch thread backs out and Hermes gets its display back.
    fn cancel_launch(self: &Shared) -> bool {
        {
            let mut state = self.state();
            if !state
                .launching
                .as_ref()
                .is_some_and(|(seq, _)| *seq == state.launch_seq)
            {
                return false;
            }
            state.launch_seq += 1;
        }
        info!("launch cancelled by the user");
        self.ui(|ui| {
            ui.set_session_busy(false);
            ui.set_screen(Screen::Apps);
        });
        self.toast("Cancelled. If the app already started on the host, quit it from the app list.");
        true
    }

    /// Quits the app running on the open host.
    pub fn quit_app(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_session_busy(true));
        let c = self.clone();
        spawn("hestia-quit", move || {
            let Some(core) = c.core() else { return };
            match core.quit(&id) {
                Ok(()) => c.toast("App closed"),
                Err(e) => c.toast(format!("Could not quit: {e}")),
            }
            c.ui(|ui| {
                ui.set_session_busy(false);
                if ui.screen() == Screen::Session {
                    ui.set_screen(Screen::Apps);
                }
            });
            c.refresh_current_running();
        });
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    pub fn diagnostics_overlay(&self) -> DiagnosticsOverlay {
        self.core().map(|core| core.diagnostics_overlay()).unwrap_or_default()
    }

    pub fn save_diagnostics_overlay(self: &Shared, preferences: DiagnosticsOverlay) {
        let c = self.clone();
        let worker = std::thread::Builder::new()
            .name("hestia-save-overlay".into())
            .spawn(move || {
                let saved = c
                    .core()
                    .is_some_and(|core| match core.set_diagnostics_overlay(preferences) {
                        Ok(()) => true,
                        Err(e) => {
                            warn!("could not save diagnostics overlay preferences: {e}");
                            false
                        }
                    });
                c.ui(move |ui| ui.diagnostics_overlay_saved(saved));
            });
        if let Err(e) = worker {
            warn!("could not start diagnostics overlay save: {e}");
            self.ui(|ui| ui.diagnostics_overlay_saved(false));
        }
    }

    /// Saves the settings form: the device's settings, or, opened from a
    /// host's page, that host's quality settings and the device's others.
    pub fn save_settings(self: &Shared) {
        let Some(core) = self.core() else { return };
        let selected = self.state().current_host.clone();
        let c = self.clone();
        self.ui(move |ui| {
            let global = core.settings();
            let base = selected
                .as_deref()
                .map(|id| core.settings_for_host(id).0)
                .unwrap_or_else(|| global.clone());
            let edited = view::read_settings_from(ui, &base);
            let result = match selected {
                // From a host's page: the quality side is that host's (its
                // profile, or its own defaults); the rest is the device's.
                Some(id) => {
                    let device = hestia_core::profiles::StreamProfile::from_settings("", &global).apply(&edited);
                    core.set_host_settings(&id, &edited)
                        .and_then(|()| core.set_settings(device))
                }
                None => core.set_settings(edited),
            };
            match result {
                Ok(()) => c.toast("Settings saved"),
                Err(e) => c.toast(format!("Could not save settings: {e}")),
            }
        });
    }

    /// Settings → a shortcut's Apply (`which`: its place in
    /// [`ShortcutAction::ALL`]). Saved at once, with the device's settings;
    /// the row goes back to the saved shortcut when `text` isn't one.
    pub fn update_shortcut(self: &Shared, which: i32, text: String) {
        let Some(core) = self.core() else { return };
        let Some(action) = ShortcutAction::from_index(which) else {
            return;
        };
        let mut settings = core.settings();
        let outcome = ShortcutBinding::parse(&text).and_then(|binding| settings.shortcuts.assign(action, binding));
        match outcome {
            Ok(()) => match core.set_settings(settings) {
                Ok(()) => self.toast("Keyboard shortcut updated"),
                Err(e) => self.toast(format!("Could not save shortcut: {e}")),
            },
            Err(why) => self.toast(format!("Invalid shortcut: {why}")),
        }
        let keys = core.settings().shortcuts;
        self.ui(move |ui| ui.set_shortcut_labels(shortcut_labels(keys)));
    }

    /// A preset picked in Settings: fill in resolution, frame rate and
    /// bitrate for this display (saved with the rest on Save).
    pub fn apply_preset(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(&preset) = Preset::ALL.get(index.max(0) as usize) else {
            return;
        };
        self.ui(move |ui| {
            let native = platform::native_display();
            let mut settings = view::read_settings_from(ui, &core.settings());
            settings.preset = preset;
            if let Some((w, h, fps, kbps)) = presets::preset_mode(preset, native.unwrap_or((1920, 1080, 60))) {
                (settings.width, settings.height, settings.fps, settings.bitrate_kbps) = (w, h, fps, kbps);
            }
            view::load_settings_into(ui, &settings, native);
        });
    }

    /// Settings → Custom resolution: adds it to the list and selects it.
    pub fn custom_resolution(self: &Shared, text: &str) {
        let Some((width, height)) = view::parse_resolution(text) else {
            self.toast("Type a size like 2560x1080 (320x200 to 7680x4320)");
            return;
        };
        let Some(core) = self.core() else { return };
        self.ui(move |ui| {
            let mut settings = view::read_settings_from(ui, &core.settings());
            (settings.width, settings.height) = (width, height);
            settings.preset = Preset::Custom;
            view::load_settings_into(ui, &settings, platform::native_display());
        });
        self.toast(format!("Resolution set to {width}×{height}; Save to keep it"));
    }

    /// Fit → Fill → Stretch, from the stream menu; remembered for next time.
    pub fn cycle_video_scale(self: &Shared) {
        let Some(core) = self.core() else { return };
        let mut settings = core.settings();
        settings.video_scale = settings.video_scale.next();
        let scale = settings.video_scale;
        self.ui(move |ui| {
            ui.set_stream_video_scale(scale.index());
            ui.set_cfg_video_scale(scale.index());
        });
        self.toast(match scale {
            hestia_core::VideoScale::Fit => "Picture: fit (whole picture, black bars if needed)",
            hestia_core::VideoScale::Fill => "Picture: fill (no bars, edges cropped)",
            hestia_core::VideoScale::Stretch => "Picture: stretch (no bars, distorted)",
        });
        spawn("hestia-settings", move || {
            if let Err(e) = core.set_settings(settings) {
                warn!("saving the picture scale failed: {e}");
            }
        });
    }
}

fn lines(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
