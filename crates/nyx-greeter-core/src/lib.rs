//! Nyx Greeter's logic without GTK.
//!
//! - [`Config`]: `/etc/nyx-greeter/config.toml` (NyxOS writes it), the theme, the
//!   wallpaper and what to show.
//! - [`State`]: who logged in last and with which session, in the greeter's cache.
//! - [`users`] and [`sessions`]: who can log in, and what they can log in to.
//! - [`flow`]: the conversation with greetd, on its own thread.

pub mod flow;
pub mod sessions;
pub mod users;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// A Nyx theme id (`nyx-dark`); the login theme is a system setting.
    pub theme: String,
    /// The picture behind the card; none: the theme's background colour.
    pub wallpaper: Option<PathBuf>,
    /// How much to darken the wallpaper (0 to 1).
    pub dim: f64,
    /// Offer the other users; off, only the last one (or a typed name) logs in.
    pub user_list: bool,
    /// Suspend, restart and shut down.
    pub power: bool,
    pub clock: bool,
    /// The session for users who haven't picked one (a desktop file name).
    pub default_session: Option<String>,
    /// Extra folders of `.desktop` sessions.
    pub session_dirs: Vec<PathBuf>,
    /// The sign-in animation for a fingerprint, face or key: `circular`, `cyberpunk`,
    /// `minimal` or `edge`.
    pub style: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "nyx-dark".into(),
            wallpaper: None,
            dim: 0.0,
            user_list: true,
            power: true,
            clock: true,
            default_session: None,
            session_dirs: vec![],
            style: "circular".into(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        env_path("NYX_GREETER_CONFIG", "/etc/nyx-greeter/config.toml")
    }

    pub fn load() -> Config {
        let path = Config::path();
        match std::fs::read_to_string(&path) {
            Ok(t) => toml::from_str(&t).unwrap_or_else(|e| {
                eprintln!("nyx-greeter: {}: {e}; using the defaults", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }
}

/// Remembered between logins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub last_user: Option<String>,
    /// Each user's last session (desktop file name).
    #[serde(default)]
    pub sessions: std::collections::BTreeMap<String, String>,
}

impl State {
    pub fn path() -> PathBuf {
        env_path("NYX_GREETER_STATE", "/var/cache/nyx-greeter/state.toml")
    }

    pub fn load() -> State {
        std::fs::read_to_string(State::path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = State::path();
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, toml::to_string(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }

    pub fn remember(&mut self, user: &str, session: &str) {
        self.last_user = Some(user.to_string());
        self.sessions.insert(user.to_string(), session.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_config_is_the_default() {
        let c: Config = toml::from_str(include_str!("../../../data/config.toml")).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c: Config = toml::from_str("theme = \"nyx-light\"\npower = false\n").unwrap();
        assert_eq!(c.theme, "nyx-light");
        assert!(!c.power);
        assert!(c.user_list);
        assert_eq!(Config::default().dim, 0.0);
    }

    #[test]
    fn state_round_trip() {
        let t = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("NYX_GREETER_STATE", t.path().join("s/state.toml")) };
        let mut s = State::default();
        s.remember("ken", "hyprland-uwsm.desktop");
        s.save();
        let back = State::load();
        assert_eq!(back.last_user.as_deref(), Some("ken"));
        assert_eq!(back.sessions["ken"], "hyprland-uwsm.desktop");
    }
}
