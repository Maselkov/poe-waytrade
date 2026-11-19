//! Map danger check (like Awakened PoE Trade's): the user marks map mods as
//! deadly, warning or good, per profile, and every checked map shows them.

use crate::item::{Item, ModKind};
use crate::trade::templatize;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

pub const PROFILES: usize = 3;

/// Marks for a fresh install, applied to every profile. Texts are GGG's stat
/// texts (they're also what map items say, apart from numbers and signs).
const DEFAULTS: &[(&str, char)] = &[
    ("Monsters reflect #% of Physical Damage", 'd'),
    ("Monsters reflect #% of Elemental Damage", 'd'),
    ("Players cannot Regenerate Life, Mana or Energy Shield", 'd'),
    ("Players have #% to all maximum Resistances", 'w'),
    ("Players have #% less Recovery Rate of Life and Energy Shield", 'w'),
    ("Cannot Leech Life from Monsters", 'w'),
    ("Monsters cannot be Leeched from", 'w'),
    ("Players are Cursed with Elemental Weakness", 'w'),
    ("Players are Cursed with Temporal Chains", 'w'),
    ("Players are Cursed with Vulnerability", 'w'),
    ("Players are Cursed with Enfeeble", 'w'),
    ("Buffs on Players expire #% faster", 'w'),
    ("Monsters are Hexproof", 'w'),
    ("Area contains two Unique Bosses", 'g'),
];

/// Map-like items, as PoE names their item class.
const MAP_CLASSES: &[&str] = &[
    "Maps",
    "Misc Map Items", // Maven's invitations
    "Contracts",
    "Heist Contracts",
    "Blueprints",
    "Heist Blueprints",
    "Expedition Logbooks",
];

pub fn is_map_like(item: &Item) -> bool {
    // Unique maps always have the same mods.
    MAP_CLASSES.contains(&item.class.as_str()) && !(item.class == "Maps" && item.rarity == "Unique")
}

/// The key a mod's marks are stored under: its text with numbers as `#` and
/// no sign before them, so "Players have -12% to all maximum Resistances"
/// and GGG's "Players have #% to all maximum Resistances" match.
pub fn mod_key(text: &str) -> String {
    templatize(text).0.replace("+#", "#").replace("-#", "#")
}

pub struct MapMod {
    pub key: String,
    pub text: String,
}

pub struct MapInfo {
    pub title: String,
    pub subtitle: String,
    pub props: Vec<(String, String)>,
    pub mods: Vec<MapMod>,
}

pub fn analyse(item: &Item) -> MapInfo {
    let mods = item
        .mods
        .iter()
        .filter(|m| m.kind == ModKind::Explicit)
        // Advanced copy marks real mods; otherwise drop the instructions and
        // notes that follow them (full sentences, unlike mods).
        .filter(|m| if item.advanced { m.headed } else { !is_boilerplate(&m.text) })
        .map(|m| MapMod { key: mod_key(&m.text), text: m.text.clone() })
        .collect();
    let subtitle = match item.map_tier {
        Some(tier) => format!("Tier {tier} · {}", item.class),
        None => item.class.clone(),
    };
    MapInfo {
        title: item.name.clone().unwrap_or_else(|| item.base.clone()),
        subtitle: if item.corrupted { format!("{subtitle} · Corrupted") } else { subtitle },
        props: item.map_props.clone(),
        mods,
    }
}

fn is_boilerplate(line: &str) -> bool {
    line.ends_with('.')
        || line.starts_with("Monster Level:")
        || line.starts_with("Note:")
        || matches!(line, "Corrupted" | "Unidentified" | "Mirrored" | "Split")
}

// ---- stored marks -----------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct Decisions {
    /// Active profile, 1-based like the I/II/III buttons.
    pub profile: usize,
    /// mod key -> one char per profile: 'd'eadly, 'w'arning, 'g'ood, '-'.
    pub mods: BTreeMap<String, String>,
}

impl Decisions {
    fn defaults() -> Decisions {
        let mods = DEFAULTS.iter().map(|(text, d)| (mod_key(text), d.to_string().repeat(PROFILES))).collect();
        Decisions { profile: 1, mods }
    }

    fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
        let path = base.join("poe-waytrade").join("map-mods.json");
        // Marks saved before the rename (poe-overlay) move over once.
        let legacy = base.join("poe-overlay").join("map-mods.json");
        if !path.exists() && legacy.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::rename(&legacy, &path) {
                Ok(()) => {
                    eprintln!("moved {} to {}", legacy.display(), path.display());
                    let _ = std::fs::remove_dir(legacy.parent().unwrap());
                }
                Err(e) => {
                    eprintln!("can't move {} to {} ({e}); reading the old file", legacy.display(), path.display());
                    return legacy;
                }
            }
        }
        path
    }

    /// The saved marks, or the defaults on first use. A file that can't be
    /// read is left alone (and reported) rather than overwritten.
    pub fn load() -> Decisions {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Decisions>(&s) {
                Ok(mut d) => {
                    d.profile = d.profile.clamp(1, PROFILES);
                    d
                }
                Err(e) => {
                    eprintln!("{}: {e}; using defaults until the next change", path.display());
                    Self::defaults()
                }
            },
            Err(_) => Self::defaults(),
        }
    }

    /// Written to a temporary file and renamed, so a crash can't leave a
    /// half-written file.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).map_err(io::Error::other)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// All profiles' marks for a mod, e.g. "dw-".
    pub fn get(&self, key: &str) -> String {
        let mut s = self.mods.get(key).cloned().unwrap_or_default();
        while s.chars().count() < PROFILES {
            s.push('-');
        }
        s
    }

    /// `profile` is 1-based; `decision` one of d/w/g/-.
    pub fn set(&mut self, profile: usize, key: &str, decision: char) -> bool {
        if !(1..=PROFILES).contains(&profile) || !"dwg-".contains(decision) || key.is_empty() {
            return false;
        }
        let mut chars: Vec<char> = self.get(key).chars().collect();
        chars[profile - 1] = decision;
        if chars.iter().all(|&c| c == '-') {
            self.mods.remove(key);
        } else {
            self.mods.insert(key.to_string(), chars.into_iter().collect());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item;

    const MAP: &str = "Item Class: Maps\nRarity: Rare\nHaunted Glade\nStrand Map\n--------\nMap Tier: 16\n\
                       Item Quantity: +85% (augmented)\nItem Rarity: +50% (augmented)\nMonster Pack Size: +30% (augmented)\n\
                       --------\nItem Level: 83\n--------\nMonster Level: 83\n--------\n\
                       Monsters reflect 18% of Physical Damage\nPlayers have -12% to all maximum Resistances\n\
                       Area contains two Unique Bosses\n--------\n\
                       Travel to this Map by using it in a personal Map Device. Maps can only be used once.\n";

    #[test]
    fn plain_map() {
        let item = item::parse(MAP).unwrap();
        assert!(is_map_like(&item));
        let info = analyse(&item);
        assert_eq!(info.title, "Haunted Glade");
        assert_eq!(info.subtitle, "Tier 16 · Maps");
        assert_eq!(info.props[0], ("Item Quantity".to_string(), "+85%".to_string()));
        let keys: Vec<_> = info.mods.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "Monsters reflect #% of Physical Damage",
                "Players have #% to all maximum Resistances",
                "Area contains two Unique Bosses",
            ]
        );
    }

    #[test]
    fn advanced_map() {
        // Every mod has a header; the instructions section has none.
        let text = MAP
            .replace(
                "Monsters reflect 18% of Physical Damage\n",
                "{ Prefix Modifier \"Punishing\" (Tier: 1) }\nMonsters reflect 18(15-18)% of Physical Damage\n",
            )
            .replace(
                "Players have -12% to all maximum Resistances\n",
                "{ Suffix Modifier \"of Exposure\" }\nPlayers have -12(-12--9)% to all maximum Resistances\n",
            )
            .replace(
                "Area contains two Unique Bosses\n",
                "{ Prefix Modifier \"Twinned\" }\nArea contains two Unique Bosses\n",
            );
        let info = analyse(&item::parse(&text).unwrap());
        let keys: Vec<_> = info.mods.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "Monsters reflect #% of Physical Damage",
                "Players have #% to all maximum Resistances",
                "Area contains two Unique Bosses",
            ]
        );
        assert_eq!(info.mods[0].text, "Monsters reflect 18% of Physical Damage");
        assert_eq!(info.mods[1].text, "Players have -12% to all maximum Resistances");
    }

    #[test]
    fn defaults_and_marks() {
        let mut d = Decisions::defaults();
        assert_eq!(d.get("Monsters reflect #% of Physical Damage"), "ddd");
        assert_eq!(d.get("Area contains two Unique Bosses"), "ggg");
        assert_eq!(d.get("Unknown mod"), "---");
        assert!(d.set(2, "Unknown mod", 'w'));
        assert_eq!(d.get("Unknown mod"), "-w-");
        assert!(d.set(2, "Unknown mod", '-'));
        assert!(!d.mods.contains_key("Unknown mod"));
        assert!(!d.set(4, "x", 'd'));
        assert!(!d.set(1, "x", 'q'));
    }

    #[test]
    fn not_maps() {
        let ring = item::parse("Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n").unwrap();
        assert!(!is_map_like(&ring));
        let unique =
            item::parse("Item Class: Maps\nRarity: Unique\nMaelström of Chaos\nAtoll Map\n--------\nMap Tier: 5\n")
                .unwrap();
        assert!(!is_map_like(&unique));
    }
}
