//! poe.ninja (PoE1) price data: fetching and an in-memory index.

use serde::Deserialize;
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

const BASE: &str = "https://poe.ninja/poe1/api";
const USER_AGENT: &str = concat!(
    "poe-overlay-daemon/",
    env!("CARGO_PKG_VERSION"),
    " (personal price-check overlay)"
);
/// Be polite: poe.ninja is a free community service.
const REQUEST_GAP: Duration = Duration::from_millis(250);

/// Categories served by the currency exchange endpoint (name -> chaos).
const EXCHANGE_TYPES: &[&str] = &[
    "Currency", "Fragment", "Oil", "Fossil", "Resonator", "Essence", "DivinationCard",
    "DeliriumOrb", "Omen", "Tattoo", "Artifact", "Runegraft", "AllflameEmber", "Astrolabe",
];

/// Categories served by the stash item endpoint (per-variant lines).
const ITEM_TYPES: &[&str] = &[
    "UniqueWeapon", "UniqueArmour", "UniqueAccessory", "UniqueFlask", "UniqueJewel",
    "UniqueTincture", "UniqueRelic", "UniqueMap", "SkillGem", "Map", "BlightedMap",
    "Invitation", "Beast",
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
    pub chaos_value: f64,
    pub divine_value: Option<f64>,
    pub listing_count: Option<u32>,
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
}

impl PriceDb {
    pub fn to_divine(&self, chaos: f64) -> f64 {
        chaos * self.divine_rate
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

/// The current softcore challenge league, falling back to Standard.
pub fn current_league() -> Result<String, String> {
    let index: IndexState = get(&format!("{BASE}/data/index-state"))?;
    Ok(index
        .economy_leagues
        .into_iter()
        .map(|l| l.name)
        .find(|n| !n.contains("Hardcore") && !n.contains("Ruthless") && n != "Standard")
        .unwrap_or_else(|| "Standard".to_string()))
}

pub fn fetch(league: &str) -> Result<PriceDb, String> {
    let mut db = PriceDb {
        league: league.to_string(),
        fetched_at: Instant::now(),
        divine_rate: 0.0,
        exchange: HashMap::new(),
        items: HashMap::new(),
        uniques_by_base: HashMap::new(),
    };
    let league_q = encode(league);
    let mut failures = 0;

    for kind in EXCHANGE_TYPES {
        let url = format!("{BASE}/economy/exchange/current/overview?league={league_q}&type={kind}");
        match get::<ExchangeOverview>(&url) {
            Ok(ov) => {
                if let Some(rate) = ov.core.rates.get("divine") {
                    db.divine_rate = *rate;
                }
                let names: HashMap<_, _> = ov.items.into_iter().map(|i| (i.id, i.name)).collect();
                for line in ov.lines {
                    if let (Some(name), Some(value)) = (names.get(&line.id), line.primary_value) {
                        db.exchange.insert(name.clone(), value);
                    }
                }
            }
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
            Ok(ov) => {
                for line in ov.lines {
                    if kind.starts_with("Unique") {
                        if let Some(base) = &line.base_type {
                            db.uniques_by_base.entry(base.clone()).or_default().push(line.clone());
                        }
                    }
                    db.items.entry(line.name.clone()).or_default().push(line);
                }
            }
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
