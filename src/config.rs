//! User settings in %APPDATA%\Pluma\config.json (shared by the window and the agent).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Storage {
    /// Settings stored in the mouse memory (recommended).
    #[default]
    Onboard,
    /// The PC applies the settings; the agent reapplies them when the mouse wakes up.
    Software,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Name of the mouse these settings belong to (None = nothing was ever saved).
    pub device: Option<String>,
    pub storage: Storage,
    pub dpi_stages: Vec<u16>,
    pub default_stage: usize,
    pub report_rate_hz: Option<u32>,
    pub background: bool,
    /// Sector of the user profile Pluma activated (to reactivate it if the mouse wakes up
    /// on the factory profile).
    pub profile_sector: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: None,
            storage: Storage::Onboard,
            dpi_stages: vec![800],
            default_stage: 0,
            report_rate_hz: None,
            background: false,
            profile_sector: None,
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("Pluma")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn backup_dir() -> PathBuf {
        Self::dir().join("backups")
    }

    pub fn load() -> Config {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::dir())?;
        let tmp = Self::dir().join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, Self::path())
    }

    pub fn active_dpi(&self) -> u16 {
        self.dpi_stages.get(self.default_stage).or(self.dpi_stages.first()).copied().unwrap_or(800)
    }
}
