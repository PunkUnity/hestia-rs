//! On-disk state: the client identity, known hosts and settings, as JSON files
//! in one data directory (XDG data dir on Linux, the app's internal storage
//! on Android).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::HostRecord;
use crate::diagnostics_overlay::DiagnosticsOverlay;
use crate::settings::StreamSettings;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredIdentity {
    pub unique_id: String,
    pub certificate_pem: String,
    pub private_key_pem: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StoredHosts {
    pub hosts: Vec<HostRecord>,
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> io::Result<Option<T>> {
        let path = self.dir.join(name);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a
    /// half-written host list behind.
    fn write<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        fs::write(&tmp, json)?;
        restrict_permissions(&tmp);
        fs::rename(&tmp, &path)
    }

    pub fn load_identity(&self) -> io::Result<Option<StoredIdentity>> {
        self.read("identity.json")
    }

    pub fn save_identity(&self, identity: &StoredIdentity) -> io::Result<()> {
        self.write("identity.json", identity)
    }

    pub fn load_hosts(&self) -> io::Result<Vec<HostRecord>> {
        Ok(self.read::<StoredHosts>("hosts.json")?.unwrap_or_default().hosts)
    }

    pub fn save_hosts(&self, hosts: &[HostRecord]) -> io::Result<()> {
        self.write("hosts.json", &StoredHosts { hosts: hosts.to_vec() })
    }

    pub fn load_settings(&self) -> io::Result<StreamSettings> {
        Ok(self.read("settings.json")?.unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &StreamSettings) -> io::Result<()> {
        self.write("settings.json", settings)
    }

    pub fn load_diagnostics_overlay(&self) -> io::Result<DiagnosticsOverlay> {
        Ok(self.read("diagnostics-overlay.json")?.unwrap_or_default())
    }

    pub fn save_diagnostics_overlay(&self, preferences: &DiagnosticsOverlay) -> io::Result<()> {
        self.write("diagnostics-overlay.json", preferences)
    }

    pub fn load_profiles(&self) -> io::Result<Vec<crate::profiles::StreamProfile>> {
        Ok(self.read("profiles.json")?.unwrap_or_default())
    }

    /// The device-wide profiles from before each host had its own, once
    /// they're copied to the hosts: renamed to `profiles.legacy.json` (kept,
    /// and never copied again). No file: nothing to do.
    pub fn archive_legacy_profiles(&self) -> io::Result<()> {
        match fs::rename(self.dir.join("profiles.json"), self.dir.join("profiles.legacy.json")) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            done => done,
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
