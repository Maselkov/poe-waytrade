//! Which game this daemon serves: Path of Exile or Path of Exile 2.
//!
//! One daemon serves one game, chosen at start by `POE_WAYTRADE_GAME`
//! (`poe1`, the default, or `poe2`; the `waytrade` launcher sets it from
//! Steam's app id). Each game has its own socket, so the layer in either
//! game finds its own daemon, and its own cache and config directories, so
//! their trade data, leagues and settings never mix.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Game {
    Poe1,
    Poe2,
}

static CURRENT: OnceLock<Game> = OnceLock::new();

/// The game this daemon serves (POE_WAYTRADE_GAME, read once).
pub fn current() -> Game {
    *CURRENT.get_or_init(|| match std::env::var("POE_WAYTRADE_GAME") {
        Ok(v) => Game::parse(&v).unwrap_or_else(|| {
            eprintln!("POE_WAYTRADE_GAME={v} isn't poe1 or poe2; serving poe1");
            Game::Poe1
        }),
        Err(_) => Game::Poe1,
    })
}

impl Game {
    pub fn parse(s: &str) -> Option<Game> {
        match s.trim() {
            "poe1" | "1" | "" => Some(Game::Poe1),
            "poe2" | "2" => Some(Game::Poe2),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Game::Poe1 => "poe1",
            Game::Poe2 => "poe2",
        }
    }

    /// The abstract socket the layer connects to (src/daemon_client.cpp
    /// picks the same name).
    pub fn socket_name(self) -> &'static str {
        match self {
            Game::Poe1 => "poe-waytrade",
            Game::Poe2 => "poe-waytrade-poe2",
        }
    }

    /// The trade API's path prefix: `/api/trade` or `/api/trade2`.
    pub fn trade_api(self) -> &'static str {
        match self {
            Game::Poe1 => "/api/trade",
            Game::Poe2 => "/api/trade2",
        }
    }

    /// Where a search opens on the trade site, before "/<league>/<id>".
    pub fn trade_site(self) -> &'static str {
        match self {
            Game::Poe1 => "https://www.pathofexile.com/trade/search",
            Game::Poe2 => "https://www.pathofexile.com/trade2/search/poe2",
        }
    }

    /// The currency prices are counted in below a Divine Orb: its trade id,
    /// and the short suffix after an amount ("12c", "40ex").
    pub fn small_unit(self) -> &'static str {
        match self {
            Game::Poe1 => "chaos",
            Game::Poe2 => "exalted",
        }
    }

    pub fn small_suffix(self) -> &'static str {
        match self {
            Game::Poe1 => "c",
            Game::Poe2 => "ex",
        }
    }

    /// The game's install directory under steamapps/common.
    pub fn steam_dir(self) -> &'static str {
        match self {
            Game::Poe1 => "Path of Exile",
            Game::Poe2 => "Path of Exile 2",
        }
    }

    /// `base`/poe-waytrade, with PoE2's things in a poe2/ directory of
    /// their own (PoE1's stay where they always were).
    pub fn dir(self, base: PathBuf) -> PathBuf {
        let dir = base.join("poe-waytrade");
        match self {
            Game::Poe1 => dir,
            Game::Poe2 => dir.join("poe2"),
        }
    }
}

/// `$XDG_CONFIG_HOME/poe-waytrade` (or PoE2's directory in it).
pub fn config_dir() -> PathBuf {
    current().dir(xdg("XDG_CONFIG_HOME", ".config"))
}

/// `$XDG_CACHE_HOME/poe-waytrade` (or PoE2's directory in it).
pub fn cache_dir() -> PathBuf {
    current().dir(xdg("XDG_CACHE_HOME", ".cache"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        assert_eq!(Game::parse("poe2"), Some(Game::Poe2));
        assert_eq!(Game::parse("poe1"), Some(Game::Poe1));
        assert_eq!(Game::parse(""), Some(Game::Poe1));
        assert_eq!(Game::parse("poe3"), None);
    }

    #[test]
    fn poe2_keeps_to_its_own_directory() {
        let base = PathBuf::from("/c");
        assert_eq!(Game::Poe1.dir(base.clone()), PathBuf::from("/c/poe-waytrade"));
        assert_eq!(Game::Poe2.dir(base), PathBuf::from("/c/poe-waytrade/poe2"));
    }
}
