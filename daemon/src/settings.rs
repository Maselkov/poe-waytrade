//! The user's settings, from the layer's settings page.
//!
//! Saved in `$XDG_CONFIG_HOME/poe-waytrade/settings.json` (next to the map
//! marks). The layer has no JSON parser, so it reads and changes them through
//! us, as `<key>\t<value>` lines (see `lines` and `set`). Missing or unknown
//! fields fall back to the defaults, so an old file keeps working.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use crate::trade::Status;

/// League settings that follow poe.ninja into each new league rather than
/// name one: the current challenge league, and its hardcore twin.
pub const AUTO_LEAGUES: &[&str] = &["auto", "auto-hardcore"];

/// The keys the layer can set, in the order they're sent.
pub const KEYS: &[&str] = &[
    "league",
    "status",
    "min_percent",
    "ui_scale",
    "hotkey.quick",
    "hotkey.locked",
    "hotkey.map_alias",
    "hotkey.settings",
    "stash_scroll",
    "cursor",
    "hotkey.displays",
    "auto_watch",
    "auto_watch_areas",
    "client_log",
    "tracking",
    "heist_watch",
    "startup_notice",
    "copy_check",
];

/// Areas where the display price check starts by itself: mob-free rooms of
/// item displays (the Templar vaults), by their exact names in Client.txt.
pub const DEFAULT_WATCH_AREAS: &[&str] = &["Zealot's Vault", "Arkhon's Vault", "Scholar's Vault", "Machinist's Vault"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// "auto" (the current challenge league, from poe.ninja), "auto-hardcore"
    /// (its hardcore twin) or a name.
    pub league: String,
    /// Which listings a search starts with: instant, available or any.
    pub status: String,
    /// A ticked filter's default minimum, as a percentage of the item's roll.
    pub min_percent: u32,
    /// The panels' size, on top of the output's scale.
    pub ui_scale: f64,
    pub hotkeys: Hotkeys,
    /// Ctrl + mouse wheel switches stash tabs (the layer sends arrow keys).
    pub stash_scroll: bool,
    /// The cursor over the panels: "game" (Wine's) or "desktop" (the
    /// cursor theme's, which keeps its size with fractional scaling).
    pub cursor: String,
    /// Entering one of `auto_watch_areas` starts pricing item displays.
    pub auto_watch: bool,
    pub auto_watch_areas: Vec<String>,
    /// The game's Client.txt; "" finds it in the Steam libraries.
    pub client_log: String,
    /// How display labels follow their tooltips: "gpu" (found in each frame
    /// as it's drawn) or "cpu" (from copies a frame or two old, predicted).
    pub tracking: String,
    /// In Heist, watch for item displays (curio displays) at a slow pace,
    /// speeding up while one is in view.
    pub heist_watch: bool,
    /// The notice at start-up that the overlay is running (and loading prices).
    pub startup_notice: bool,
    /// The user's own Ctrl+C on an item opens a quick look, like Ctrl+D.
    pub copy_check: bool,
}

/// Each a key combination as the layer matches it: modifiers and an evdev
/// key code, "ctrl+alt+32" (Ctrl+Alt+D, wherever the layout puts D).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Hotkeys {
    pub quick: String,
    pub locked: String,
    /// The old map check key; "" turns it off.
    pub map_alias: String,
    pub settings: String,
    /// Prices the item displays on screen until the area changes; "" turns
    /// it off.
    pub displays: String,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            league: "auto".to_string(),
            status: "instant".to_string(),
            min_percent: 90,
            ui_scale: 1.0,
            hotkeys: Hotkeys::default(),
            stash_scroll: true,
            cursor: "game".to_string(),
            auto_watch: true,
            auto_watch_areas: DEFAULT_WATCH_AREAS.iter().map(|a| a.to_string()).collect(),
            client_log: String::new(),
            tracking: "gpu".to_string(),
            heist_watch: true,
            startup_notice: true,
            copy_check: false,
        }
    }
}

impl Default for Hotkeys {
    /// The layer has the same defaults (settings_page.cpp).
    fn default() -> Hotkeys {
        // PoE2 can move with WASD, and the game sees our hotkeys too: Ctrl+D
        // would walk away from the stash. The key under Esc (41) is free
        // there. No map check alias or item displays in PoE2.
        if crate::game::current() == crate::game::Game::Poe2 {
            return Hotkeys {
                quick: "ctrl+41".to_string(),
                locked: "ctrl+alt+41".to_string(),
                map_alias: String::new(),
                settings: "ctrl+alt+31".to_string(),
                displays: String::new(),
            };
        }
        Hotkeys {
            quick: "ctrl+32".to_string(),
            locked: "ctrl+alt+32".to_string(),
            map_alias: "alt+32".to_string(),
            settings: "ctrl+alt+31".to_string(),
            displays: "ctrl+alt+33".to_string(),
        }
    }
}

// What searches read: set from the loaded settings and on each change.
static MIN_PERCENT: AtomicU32 = AtomicU32::new(90);
static STATUS: AtomicU8 = AtomicU8::new(0);

/// The default filter minimum, as a fraction of the item's roll.
pub fn min_fraction() -> f64 {
    MIN_PERCENT.load(Ordering::Relaxed) as f64 / 100.0
}

/// The listings a search starts with.
pub fn default_status() -> Status {
    match STATUS.load(Ordering::Relaxed) {
        1 => Status::Available,
        2 => Status::Any,
        _ => Status::Instant,
    }
}

impl Settings {
    fn path() -> PathBuf {
        crate::game::config_dir().join("settings.json")
    }

    /// The saved settings, or the defaults. A file that can't be read is
    /// left alone (and reported) rather than overwritten.
    pub fn load() -> Settings {
        let path = Self::path();
        let settings = match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Settings>(&s) {
                Ok(loaded) => loaded.validated(),
                Err(e) => {
                    eprintln!("{}: {e}; using defaults until the next change", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        };
        settings.apply();
        settings
    }

    /// Written to a temporary file and renamed, like the map marks.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).map_err(io::Error::other)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// Makes searches use these.
    pub fn apply(&self) {
        MIN_PERCENT.store(self.min_percent, Ordering::Relaxed);
        let status = match Status::parse(&self.status) {
            Status::Instant => 0,
            Status::Available => 1,
            Status::Any => 2,
        };
        STATUS.store(status, Ordering::Relaxed);
    }

    /// A hand-edited file's bad values replaced by the defaults.
    fn validated(self) -> Settings {
        let mut out = Settings::default();
        for key in KEYS {
            let value = self.get(key);
            if let Err(e) = out.set(key, &value) {
                eprintln!("settings.json: {key}: {e}; using the default");
            }
        }
        out
    }

    pub fn get(&self, key: &str) -> String {
        match key {
            "league" => self.league.clone(),
            "status" => self.status.clone(),
            "min_percent" => self.min_percent.to_string(),
            "ui_scale" => format_scale(self.ui_scale),
            "hotkey.quick" => self.hotkeys.quick.clone(),
            "hotkey.locked" => self.hotkeys.locked.clone(),
            "hotkey.map_alias" => self.hotkeys.map_alias.clone(),
            "hotkey.settings" => self.hotkeys.settings.clone(),
            "stash_scroll" => if self.stash_scroll { "on" } else { "off" }.to_string(),
            "cursor" => self.cursor.clone(),
            "hotkey.displays" => self.hotkeys.displays.clone(),
            "auto_watch" => if self.auto_watch { "on" } else { "off" }.to_string(),
            "auto_watch_areas" => self.auto_watch_areas.join(";"),
            "client_log" => self.client_log.clone(),
            "tracking" => self.tracking.clone(),
            "heist_watch" => if self.heist_watch { "on" } else { "off" }.to_string(),
            "startup_notice" => if self.startup_notice { "on" } else { "off" }.to_string(),
            "copy_check" => if self.copy_check { "on" } else { "off" }.to_string(),
            _ => String::new(),
        }
    }

    /// Checks and stores one value (tidied: "Ctrl+D" style combos are put in
    /// one order, scales rounded).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        match key {
            "league" => {
                if value.is_empty() || value.contains(['\t', '\n']) {
                    return Err("Bad league name".to_string());
                }
                self.league = value.to_string();
            }
            "status" => {
                if !["instant", "available", "any"].contains(&value) {
                    return Err("Status is instant, available or any".to_string());
                }
                self.status = value.to_string();
            }
            "min_percent" => match value.parse::<u32>() {
                Ok(n) if (50..=100).contains(&n) => self.min_percent = n,
                _ => return Err("The default minimum is 50 to 100%".to_string()),
            },
            "ui_scale" => match value.parse::<f64>() {
                Ok(n) if (0.5..=2.0).contains(&n) => self.ui_scale = (n * 20.0).round() / 20.0,
                _ => return Err("The panel size is 50 to 200%".to_string()),
            },
            "hotkey.quick" | "hotkey.locked" | "hotkey.settings" => {
                let combo = parse_combo(value)?;
                match key {
                    "hotkey.quick" => self.hotkeys.quick = combo,
                    "hotkey.locked" => self.hotkeys.locked = combo,
                    _ => self.hotkeys.settings = combo,
                }
            }
            "hotkey.map_alias" => {
                self.hotkeys.map_alias = if value.is_empty() { String::new() } else { parse_combo(value)? };
            }
            "hotkey.displays" => {
                self.hotkeys.displays = if value.is_empty() { String::new() } else { parse_combo(value)? };
            }
            "auto_watch" => match value {
                "on" => self.auto_watch = true,
                "off" => self.auto_watch = false,
                _ => return Err("Auto watch is on or off".to_string()),
            },
            "auto_watch_areas" => {
                self.auto_watch_areas =
                    value.split(';').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect();
            }
            "client_log" => {
                if !(value.is_empty() || value.starts_with('/')) {
                    return Err("Client.txt's path is absolute, or empty to find it".to_string());
                }
                self.client_log = value.to_string();
            }
            "heist_watch" => match value {
                "on" => self.heist_watch = true,
                "off" => self.heist_watch = false,
                _ => return Err("Heist watch is on or off".to_string()),
            },
            "startup_notice" => match value {
                "on" => self.startup_notice = true,
                "off" => self.startup_notice = false,
                _ => return Err("The start-up notice is on or off".to_string()),
            },
            "copy_check" => match value {
                "on" => self.copy_check = true,
                "off" => self.copy_check = false,
                _ => return Err("Ctrl+C price checks are on or off".to_string()),
            },
            "tracking" => {
                if !["gpu", "cpu"].contains(&value) {
                    return Err("Tracking is gpu or cpu".to_string());
                }
                self.tracking = value.to_string();
            }
            "stash_scroll" => match value {
                "on" => self.stash_scroll = true,
                "off" => self.stash_scroll = false,
                _ => return Err("Stash scroll is on or off".to_string()),
            },
            "cursor" => {
                if !["game", "desktop"].contains(&value) {
                    return Err("The cursor is game or desktop".to_string());
                }
                self.cursor = value.to_string();
            }
            _ => return Err(format!("Unknown setting {key}")),
        }
        Ok(())
    }

    /// `setting <key> <value>` lines for the layer.
    pub fn lines(&self) -> String {
        KEYS.iter().map(|k| format!("setting\t{k}\t{}\n", self.get(k))).collect()
    }
}

fn format_scale(n: f64) -> String {
    let s = format!("{n:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// "alt+ctrl+32" -> "ctrl+alt+32": modifiers in a fixed order, then the key.
fn parse_combo(value: &str) -> Result<String, String> {
    const MODS: [&str; 4] = ["ctrl", "alt", "shift", "super"];
    let bad = || format!("Bad key combination {value:?}");
    let mut parts: Vec<&str> = value.split('+').map(str::trim).collect();
    let code: u32 = parts.pop().and_then(|c| c.parse().ok()).ok_or_else(bad)?;
    // evdev key codes: KEY_ESC (1) up to KEY_MAX; modifiers can't be the key.
    const MODIFIER_CODES: [u32; 8] = [29, 97, 56, 100, 42, 54, 125, 126];
    if !(1..0x300).contains(&code) || MODIFIER_CODES.contains(&code) {
        return Err(bad());
    }
    let mut held = [false; 4];
    for p in parts {
        let i = MODS.iter().position(|m| m.eq_ignore_ascii_case(p)).ok_or_else(bad)?;
        if held[i] {
            return Err(bad());
        }
        held[i] = true;
    }
    let mut out: Vec<String> = MODS.iter().zip(held).filter(|(_, h)| *h).map(|(m, _)| m.to_string()).collect();
    out.push(code.to_string());
    Ok(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_are_tidied_and_checked() {
        assert_eq!(parse_combo("alt+ctrl+32").unwrap(), "ctrl+alt+32");
        assert_eq!(parse_combo("Shift+59").unwrap(), "shift+59");
        assert_eq!(parse_combo("33").unwrap(), "33");
        for bad in ["", "ctrl", "ctrl+ctrl+32", "hyper+32", "ctrl+29", "ctrl+0", "ctrl+99999", "ctrl+d"] {
            assert!(parse_combo(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn set_checks_values() {
        let mut s = Settings::default();
        s.set("min_percent", "80").unwrap();
        s.set("ui_scale", "1.234").unwrap();
        s.set("status", "available").unwrap();
        s.set("hotkey.quick", "alt+shift+33").unwrap();
        s.set("hotkey.map_alias", "").unwrap();
        s.set("league", "auto-hardcore").unwrap();
        assert_eq!(s.get("league"), "auto-hardcore");
        s.set("league", "Standard").unwrap();
        s.set("stash_scroll", "off").unwrap();
        s.set("cursor", "desktop").unwrap();
        s.set("hotkey.displays", "").unwrap();
        s.set("auto_watch", "off").unwrap();
        s.set("auto_watch_areas", " Zealot's Vault ;; Arkhon's Vault").unwrap();
        s.set("client_log", "/games/poe/logs/Client.txt").unwrap();
        s.set("tracking", "cpu").unwrap();
        s.set("heist_watch", "off").unwrap();
        assert!(!s.heist_watch);
        s.set("startup_notice", "off").unwrap();
        assert!(!s.startup_notice);
        assert_eq!(s.get("startup_notice"), "off");
        assert_eq!(s.get("copy_check"), "off");
        s.set("copy_check", "on").unwrap();
        assert!(s.copy_check);
        assert_eq!(s.get("tracking"), "cpu");
        assert_eq!(s.auto_watch_areas, ["Zealot's Vault", "Arkhon's Vault"]);
        assert_eq!(s.get("auto_watch_areas"), "Zealot's Vault;Arkhon's Vault");
        assert!(!s.auto_watch);
        assert_eq!(s.hotkeys.displays, "");
        assert_eq!(s.min_percent, 80);
        assert_eq!(s.get("cursor"), "desktop");
        assert!(!s.stash_scroll);
        assert_eq!(s.get("stash_scroll"), "off");
        assert_eq!(s.get("ui_scale"), "1.25");
        assert_eq!(s.hotkeys.quick, "alt+shift+33");
        assert_eq!(s.hotkeys.map_alias, "");
        for (key, value) in [
            ("min_percent", "20"),
            ("min_percent", "x"),
            ("ui_scale", "3"),
            ("status", "online"),
            ("hotkey.quick", ""),
            ("league", ""),
            ("stash_scroll", "yes"),
            ("cursor", "theme"),
            ("auto_watch", "yes"),
            ("client_log", "logs/Client.txt"),
            ("tracking", "fast"),
            ("heist_watch", "maybe"),
            ("startup_notice", "maybe"),
            ("copy_check", "maybe"),
            ("nonsense", "1"),
        ] {
            let before = s.clone();
            assert!(s.set(key, value).is_err(), "{key}={value} was accepted");
            assert_eq!(s, before);
        }
    }

    #[test]
    fn partial_and_bad_files_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"min_percent": 85, "hotkeys": {"quick": "alt+33"}}"#).unwrap();
        let s = s.validated();
        assert_eq!(s.min_percent, 85);
        assert_eq!(s.hotkeys.quick, "alt+33");
        assert_eq!(s.hotkeys.locked, Hotkeys::default().locked);
        assert_eq!(s.league, "auto");
        assert!(s.stash_scroll, "missing from older files: on");
        assert_eq!(s.cursor, "game", "missing from older files: the game's");
        assert!(s.auto_watch, "missing from older files: on");
        assert!(!s.copy_check, "missing from older files: off");
        assert_eq!(s.auto_watch_areas.len(), DEFAULT_WATCH_AREAS.len());
        assert_eq!(s.hotkeys.displays, "ctrl+alt+33");

        let s: Settings = serde_json::from_str(r#"{"stash_scroll": false}"#).unwrap();
        assert!(!s.validated().stash_scroll);

        let s: Settings = serde_json::from_str(r#"{"min_percent": 5, "ui_scale": 9, "status": "x"}"#).unwrap();
        assert_eq!(s.validated(), Settings::default());
    }

    #[test]
    fn lines_round_trip() {
        let mut s = Settings::default();
        s.set("ui_scale", "1.5").unwrap();
        let lines = s.lines();
        assert!(lines.contains("setting\tui_scale\t1.5\n"), "{lines}");
        assert!(lines.contains("setting\thotkey.locked\tctrl+alt+32\n"), "{lines}");
        let mut back = Settings::default();
        for line in lines.lines() {
            let mut f = line.split('\t').skip(1);
            back.set(f.next().unwrap(), f.next().unwrap()).unwrap();
        }
        assert_eq!(back, s);
    }
}
