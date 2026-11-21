//! poe.ninja (PoE1) price data: fetching and an in-memory index.

use serde::Deserialize;
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

const BASE: &str = "https://poe.ninja/poe1/api";
pub const USER_AGENT: &str =
    concat!("poe-waytrade-daemon/", env!("WAYTRADE_VERSION"), " (personal price-check overlay)");
/// Be polite: poe.ninja is a free community service.
const REQUEST_GAP: Duration = Duration::from_millis(250);

/// Categories served by the currency exchange endpoint (name -> chaos).
const EXCHANGE_TYPES: &[&str] = &[
    "Currency",
    "Fragment",
    "Oil",
    "Fossil",
    "Resonator",
    "Essence",
    "DivinationCard",
    "DeliriumOrb",
    "Omen",
    "Tattoo",
    "Artifact",
    "Runegraft",
    "AllflameEmber",
    "Astrolabe",
];

/// Categories served by the stash item endpoint (per-variant lines).
const ITEM_TYPES: &[&str] = &[
    "UniqueWeapon",
    "UniqueArmour",
    "UniqueAccessory",
    "UniqueFlask",
    "UniqueJewel",
    "UniqueTincture",
    "UniqueRelic",
    "UniqueMap",
    "SkillGem",
    "Map",
    "BlightedMap",
    "Invitation",
    "Beast",
    "Viscera",
    "ClusterJewel",
    "BaseType",
];

#[derive(Deserialize)]
struct IndexState {
    #[serde(rename = "economyLeagues")]
    economy_leagues: Vec<League>,
}

#[derive(Deserialize)]
struct League {
    name: String,
}

#[derive(Deserialize)]
struct ExchangeOverview {
    core: ExchangeCore,
    lines: Vec<ExchangeLine>,
    items: Vec<ExchangeItem>,
}

#[derive(Deserialize)]
struct ExchangeCore {
    /// e.g. {"divine": 0.0044}: units of that currency per chaos.
    rates: HashMap<String, f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeLine {
    id: String,
    primary_value: Option<f64>,
}

#[derive(Deserialize)]
struct ExchangeItem {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct ItemOverview {
    lines: Vec<ItemLine>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ItemLine {
    pub name: String,
    pub base_type: Option<String>,
    pub variant: Option<String>,
    pub links: Option<u32>,
    pub gem_level: Option<u32>,
    pub gem_quality: Option<u32>,
    pub corrupted: Option<bool>,
    /// Item level for base types (82-86, 86 meaning 86+) and cluster
    /// jewels (the bracket: 1, 50, 68, 75 or 84).
    pub level_required: Option<u32>,
    pub chaos_value: f64,
    pub divine_value: Option<f64>,
    pub listing_count: Option<u32>,
    /// The item's art on web.poecdn.com (art.rs tells unidentified uniques
    /// apart by it).
    pub icon: Option<String>,
}

pub struct PriceDb {
    pub league: String,
    pub fetched_at: Instant,
    /// Divine Orbs per chaos.
    pub divine_rate: f64,
    /// Exchange items by name -> chaos value.
    pub exchange: HashMap<String, f64>,
    /// Item lines by name (unique name, gem name, map name, ...).
    pub items: HashMap<String, Vec<ItemLine>>,
    /// Unique item lines by base type, for unidentified uniques.
    pub uniques_by_base: HashMap<String, Vec<ItemLine>>,
    /// Plain bases of item level 82+ by base type, one line per item level
    /// and influences (the variant: "Hunter", "Shaper/Elder", none).
    pub bases: HashMap<String, Vec<ItemLine>>,
    /// Cluster jewels by base type; the name is the enchant ("10% increased
    /// Spell Damage") and the variant the passive count ("8 passives").
    pub clusters: HashMap<String, Vec<ItemLine>>,
}

impl PriceDb {
    fn new(league: &str) -> PriceDb {
        PriceDb {
            league: league.to_string(),
            fetched_at: Instant::now(),
            divine_rate: 0.0,
            exchange: HashMap::new(),
            items: HashMap::new(),
            uniques_by_base: HashMap::new(),
            bases: HashMap::new(),
            clusters: HashMap::new(),
        }
    }

    pub fn to_divine(&self, chaos: f64) -> f64 {
        chaos * self.divine_rate
    }

    fn add_exchange(&mut self, ov: ExchangeOverview) {
        if let Some(rate) = ov.core.rates.get("divine") {
            self.divine_rate = *rate;
        }
        let names: HashMap<_, _> = ov.items.into_iter().map(|i| (i.id, i.name)).collect();
        for line in ov.lines {
            if let (Some(name), Some(value)) = (names.get(&line.id), line.primary_value) {
                self.exchange.insert(name.clone(), value);
            }
        }
    }

    /// `kind`: the poe.ninja type the lines came from ("UniqueArmour", ...).
    fn add_items(&mut self, kind: &str, ov: ItemOverview) {
        // Named by their base or enchant, so they get maps of their own:
        // `items` is looked up by name, and a rare "Vaal Regalia" mustn't
        // find the plain base's price there.
        let by_base = match kind {
            "BaseType" => Some(&mut self.bases),
            "ClusterJewel" => Some(&mut self.clusters),
            _ => None,
        };
        if let Some(map) = by_base {
            for line in ov.lines {
                let base = line.base_type.clone().unwrap_or_else(|| line.name.clone());
                map.entry(base).or_default().push(line);
            }
            return;
        }
        for line in ov.lines {
            if kind.starts_with("Unique")
                && let Some(base) = &line.base_type
            {
                self.uniques_by_base.entry(base.clone()).or_default().push(line.clone());
            }
            self.items.entry(line.name.clone()).or_default().push(line);
        }
    }
}

fn get<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    resp.into_json().map_err(|e| format!("{url}: bad JSON: {e}"))
}

/// The leagues poe.ninja has prices for, as it orders them (challenge
/// leagues first).
pub fn leagues() -> Result<Vec<String>, String> {
    let index: IndexState = get(&format!("{BASE}/data/index-state"))?;
    Ok(index.economy_leagues.into_iter().map(|l| l.name).collect())
}

/// The current challenge league, falling back to Standard; or with
/// `hardcore`, its hardcore twin ("Hardcore Covens"), falling back to
/// Hardcore.
pub fn pick_league(names: &[String], hardcore: bool) -> String {
    let (permanent, challenge): (&str, fn(&str) -> bool) = if hardcore {
        ("Hardcore", |n| n.starts_with("Hardcore ") && !n.contains("Ruthless"))
    } else {
        ("Standard", |n| !n.contains("Hardcore") && !n.contains("Ruthless") && n != "Standard")
    };
    names.iter().find(|n| challenge(n)).cloned().unwrap_or_else(|| permanent.to_string())
}

/// A league setting that wants hardcore: "auto-hardcore", or a hardcore
/// league's name. When a named league ends, its "Current" stays hardcore.
pub fn is_hardcore(setting: &str) -> bool {
    setting == "auto-hardcore"
        || setting == "Hardcore"
        || setting.starts_with("Hardcore ")
        || setting.starts_with("HC ")
}

pub fn fetch(league: &str) -> Result<PriceDb, String> {
    let mut db = PriceDb::new(league);
    let league_q = encode(league);
    let mut failures = 0;

    for kind in EXCHANGE_TYPES {
        let url = format!("{BASE}/economy/exchange/current/overview?league={league_q}&type={kind}");
        match get::<ExchangeOverview>(&url) {
            Ok(ov) => db.add_exchange(ov),
            Err(e) => {
                eprintln!("poe.ninja: {e}");
                failures += 1;
            }
        }
        thread::sleep(REQUEST_GAP);
    }

    for kind in ITEM_TYPES {
        let url = format!("{BASE}/economy/stash/current/item/overview?league={league_q}&type={kind}");
        match get::<ItemOverview>(&url) {
            Ok(ov) => db.add_items(kind, ov),
            Err(e) => {
                eprintln!("poe.ninja: {e}");
                failures += 1;
            }
        }
        thread::sleep(REQUEST_GAP);
    }

    if failures == EXCHANGE_TYPES.len() + ITEM_TYPES.len() {
        return Err("every poe.ninja request failed".to_string());
    }
    if db.divine_rate == 0.0 {
        return Err("no divine rate in poe.ninja data".to_string());
    }
    Ok(db)
}

/// Minimal URL encoding; league names are plain words and spaces.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// poe.ninja replies captured by fixtures/refresh.py.
    const EXCHANGE: &str = include_str!("../fixtures/ninja-exchange-currency.json");
    const ITEMS: &[(&str, &str)] = &[
        ("UniqueArmour", include_str!("../fixtures/ninja-unique-armour.json")),
        ("SkillGem", include_str!("../fixtures/ninja-skill-gem.json")),
        ("Map", include_str!("../fixtures/ninja-map.json")),
        ("Viscera", include_str!("../fixtures/ninja-viscera.json")),
        ("ClusterJewel", include_str!("../fixtures/ninja-cluster-jewel.json")),
        ("BaseType", include_str!("../fixtures/ninja-base-type.json")),
    ];

    /// A price list built from the fixtures the way `fetch` builds it.
    pub(crate) fn fixture_db() -> PriceDb {
        let mut db = PriceDb::new("Fixture");
        db.add_exchange(serde_json::from_str(EXCHANGE).expect("exchange overview"));
        for (kind, json) in ITEMS {
            let ov: ItemOverview = serde_json::from_str(json).unwrap_or_else(|e| panic!("{kind} overview: {e}"));
            db.add_items(kind, ov);
        }
        db
    }

    /// The fixture's own lines for one name, as raw JSON.
    pub(crate) fn fixture_lines(kind: &str, name: &str) -> Vec<serde_json::Value> {
        let json = ITEMS.iter().find(|(k, _)| *k == kind).unwrap().1;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["lines"].as_array().unwrap().iter().filter(|l| l["name"] == name).cloned().collect()
    }

    #[test]
    fn league_from_index_state() {
        let index: IndexState = serde_json::from_str(include_str!("../fixtures/ninja-index-state.json")).unwrap();
        let names: Vec<String> = index.economy_leagues.iter().map(|l| l.name.clone()).collect();
        let league = pick_league(&names, false);
        assert!(names.contains(&league), "{league} isn't one of {names:?}");
        assert!(!league.contains("Hardcore") && league != "Standard", "picked {league} from {names:?}");
        let hc = pick_league(&names, true);
        assert!(names.contains(&hc), "{hc} isn't one of {names:?}");
        assert!(hc.starts_with("Hardcore ") && hc.ends_with(&league), "picked {hc} from {names:?}");

        let only = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let names = only(&["Hardcore Covens", "Ruthless Covens", "Standard", "Hardcore"]);
        assert_eq!(pick_league(&names, false), "Standard");
        assert_eq!(pick_league(&names, true), "Hardcore Covens");
        let names = only(&["Covens", "Hardcore Ruthless Covens", "Standard", "Hardcore"]);
        assert_eq!(pick_league(&names, false), "Covens");
        assert_eq!(pick_league(&names, true), "Hardcore");
    }

    #[test]
    fn hardcore_settings() {
        for s in ["auto-hardcore", "Hardcore", "Hardcore Covens", "HC Ruthless Covens"] {
            assert!(is_hardcore(s), "{s}");
        }
        for s in ["auto", "Standard", "Covens", "Ruthless Covens", "Hardcoreish (PL1)"] {
            assert!(!is_hardcore(s), "{s}");
        }
    }

    #[test]
    fn exchange_rates() {
        let db = fixture_db();
        let rate: serde_json::Value = serde_json::from_str(EXCHANGE).unwrap();
        assert_eq!(db.divine_rate, rate["core"]["rates"]["divine"].as_f64().unwrap());
        assert!(db.divine_rate > 0.0 && db.divine_rate < 1.0, "divine rate {}", db.divine_rate);
        // Names come from the reply's item list, values from its lines.
        assert_eq!(db.exchange.get("Chaos Orb"), Some(&1.0));
        let divine = db.exchange["Divine Orb"];
        assert!((db.to_divine(divine) - 1.0).abs() < 0.05, "a divine is {} divines", db.to_divine(divine));
        assert!(db.exchange["Mirror of Kalandra"] > divine);
    }

    #[test]
    fn item_lines() {
        let db = fixture_db();
        // Every line of every reply is kept: a field that stops being sent
        // (or changes type) fails the whole reply instead.
        let sent: usize = ITEMS
            .iter()
            .map(|(_, json)| {
                serde_json::from_str::<serde_json::Value>(json).unwrap()["lines"].as_array().unwrap().len()
            })
            .sum();
        let kept = [&db.items, &db.bases, &db.clusters].iter().flat_map(|m| m.values()).map(Vec::len).sum::<usize>();
        assert_eq!(kept, sent);
        let gem = &db.items["Arc"];
        assert!(gem.iter().all(|l| l.gem_level.is_some()), "gem lines without a level");
        assert!(gem.iter().any(|l| l.corrupted == Some(true)) && gem.iter().any(|l| l.gem_quality.is_some()));
        assert!(db.items["Queen of the Forest"].iter().any(|l| l.links == Some(6)));
    }

    #[test]
    fn uniques_by_base() {
        let db = fixture_db();
        let robes: std::collections::HashSet<&str> =
            db.uniques_by_base["Spidersilk Robe"].iter().map(|l| l.name.as_str()).collect();
        for name in ["Soul Mantle", "The Covenant", "Replica Covenant"] {
            assert!(robes.contains(name), "{name} missing from {robes:?}");
        }
        // Only uniques: gems, maps and viscera aren't looked up by base.
        assert!(!db.uniques_by_base.values().flatten().any(|l| l.name == "Arc" || l.name == "Nightmare Map"));
    }

    #[test]
    fn league_encoding() {
        assert_eq!(encode("Covens"), "Covens");
        assert_eq!(encode("Hardcore Covens"), "Hardcore%20Covens");
        assert_eq!(encode("Mirage (PL123)"), "Mirage%20%28PL123%29");
    }
}
