//! poe.ninja price data: fetching and an in-memory index.
//!
//! PoE1's prices are in chaos. PoE2's poe.ninja counts in divines; its
//! prices are turned into Exalted Orbs here, PoE2's small unit, so the rest
//! of the daemon reads them like PoE1's chaos values ("chaos" below is the
//! game's small unit: `game::Game::small_unit`).

use crate::game::{self, Game};
use serde::Deserialize;
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn base() -> &'static str {
    match game::current() {
        Game::Poe1 => "https://poe.ninja/poe1/api",
        Game::Poe2 => "https://poe.ninja/poe2/api",
    }
}
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
    "Thread",
];

/// PoE2's exchange categories.
const POE2_EXCHANGE_TYPES: &[&str] = &[
    "Currency",
    "Fragments",
    "Runes",
    "Essences",
    "Ritual",
    "Abyss",
    "UncutGems",
    "LineageSupportGems",
    "Idols",
    "Expedition",
    "Delirium",
    "Breach",
    "Verisium",
    "Ultimatum",
    "SoulCores",
];

fn exchange_types() -> &'static [&'static str] {
    match game::current() {
        Game::Poe1 => EXCHANGE_TYPES,
        Game::Poe2 => POE2_EXCHANGE_TYPES,
    }
}

/// PoE2's stash item categories: uniques, priced in divines.
const POE2_ITEM_TYPES: &[&str] = &[
    "UniqueWeapons",
    "UniqueArmours",
    "UniqueAccessories",
    "UniqueFlasks",
    "UniqueCharms",
    "UniqueJewels",
    "UniqueTablets",
];

fn item_types() -> &'static [&'static str] {
    match game::current() {
        Game::Poe1 => ITEM_TYPES,
        Game::Poe2 => POE2_ITEM_TYPES,
    }
}

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
    "BlightRavagedMap",
    "Invitation",
    "Beast",
    "Viscera",
    "ClusterJewel",
    "BaseType",
    "Wombgift",
    "ValdoMap",
    "Corpse",
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
    /// e.g. {"divine": 0.0044}: units of that currency per `primary`.
    rates: HashMap<String, f64>,
    /// The currency the lines' values are in: chaos (PoE1), divine (PoE2).
    primary: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExchangeLine {
    id: String,
    primary_value: Option<f64>,
    #[serde(default)]
    sparkline: Option<Spark>,
}

#[derive(Deserialize)]
struct ExchangeItem {
    id: String,
    name: String,
    /// What poe.ninja's details (and the item's history) go by: "divine-orb".
    #[serde(rename = "detailsId", default)]
    details_id: Option<String>,
}

#[derive(Deserialize)]
struct ItemOverview {
    lines: Vec<ItemLine>,
    /// PoE2's: the currency `primaryValue`s are in, and its rates.
    core: Option<ExchangeCore>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ItemLine {
    /// poe.ninja's id for the line, which its history goes by.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    /// The poe.ninja type the line came from ("UniqueArmour"), set when read.
    #[serde(skip)]
    pub kind: String,
    pub name: String,
    pub base_type: Option<String>,
    pub variant: Option<String>,
    pub links: Option<u32>,
    pub gem_level: Option<u32>,
    pub gem_quality: Option<u32>,
    pub corrupted: Option<bool>,
    /// Item level for base types (82-86, 86 meaning 86+), cluster jewels
    /// (the bracket: 1, 50, 68, 75 or 84) and Wombgifts (40, 68, 75 or 84).
    pub level_required: Option<u32>,
    /// In the game's small unit (PoE2's lines have only `primary_value`,
    /// turned into this when read).
    #[serde(default)]
    pub chaos_value: f64,
    pub divine_value: Option<f64>,
    /// PoE2: the value in the overview's primary currency (divines).
    #[serde(default)]
    primary_value: Option<f64>,
    pub listing_count: Option<u32>,
    /// The price over the last week.
    #[serde(default)]
    pub spark_line: Option<Spark>,
    /// The item's art on web.poecdn.com (art.rs tells unidentified uniques
    /// apart by it).
    pub icon: Option<String>,
    /// A unique's mods as poe.ninja words them ("+(15-25)% to Cold
    /// Resistance"): what tells its variants apart.
    #[serde(default)]
    pub implicit_modifiers: Vec<NinjaMod>,
    #[serde(default)]
    pub explicit_modifiers: Vec<NinjaMod>,
    /// Foulborn uniques' mutated mods.
    #[serde(default)]
    pub mutated_modifiers: Vec<NinjaMod>,
}

/// A price's last week as poe.ninja gives it: a point a day, each the
/// change in percent against the price at the start of the week (`None`
/// on a day with no listings), and the change over the whole week.
/// Percentages, so the same whatever currency the price is in.
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Spark {
    #[serde(default)]
    pub total_change: f64,
    #[serde(default)]
    pub data: Vec<Option<f64>>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct NinjaMod {
    pub text: String,
}

impl ItemLine {
    /// Where poe.ninja keeps the line's whole history (history.rs), as the
    /// layer sends it back: "item:<type>:<id>".
    pub fn history_ref(&self) -> Option<String> {
        let id = match self.id.as_ref()? {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) if !s.is_empty() => s.clone(),
            _ => return None,
        };
        (!self.kind.is_empty()).then(|| format!("item:{}:{id}", self.kind))
    }

    /// Every mod line, implicit, explicit and mutated (a multi-line mod
    /// split into its lines, as an item's text has them).
    pub fn mod_lines(&self) -> impl Iterator<Item = &str> {
        [&self.implicit_modifiers, &self.explicit_modifiers, &self.mutated_modifiers]
            .into_iter()
            .flatten()
            .flat_map(|m| m.text.lines())
    }
}

pub struct PriceDb {
    pub league: String,
    pub fetched_at: Instant,
    /// Divine Orbs per chaos.
    pub divine_rate: f64,
    /// Exchange items by name -> chaos value.
    pub exchange: HashMap<String, f64>,
    /// Exchange items by name -> their last week.
    pub exchange_spark: HashMap<String, Spark>,
    /// Exchange items by name -> where their whole history is (history.rs):
    /// "exchange:<type>:<details id>".
    pub exchange_history: HashMap<String, String>,
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
    /// Valdo maps by reward ("Foil Mageblood"), one line per map listed: the
    /// name is the map's own, and its mods make its price.
    pub valdo: HashMap<String, Vec<ItemLine>>,
}

impl PriceDb {
    fn new(league: &str) -> PriceDb {
        PriceDb {
            league: league.to_string(),
            fetched_at: Instant::now(),
            divine_rate: 0.0,
            exchange: HashMap::new(),
            exchange_spark: HashMap::new(),
            exchange_history: HashMap::new(),
            items: HashMap::new(),
            uniques_by_base: HashMap::new(),
            bases: HashMap::new(),
            clusters: HashMap::new(),
            valdo: HashMap::new(),
        }
    }

    pub fn to_divine(&self, chaos: f64) -> f64 {
        chaos * self.divine_rate
    }

    /// `kind`: the poe.ninja type the overview is of ("Currency").
    /// `small`: the game's small unit, the values' unit here.
    fn add_exchange(&mut self, kind: &str, ov: ExchangeOverview, small: &str) {
        // An empty category (PoE2's Ultimatum, between leagues) has no rates.
        if ov.lines.is_empty() {
            return;
        }
        // Units of a currency per one of the reply's primary currency.
        let primary = ov.core.primary.as_deref().unwrap_or(small);
        let per = |c: &str| if c == primary { Some(1.0) } else { ov.core.rates.get(c).copied().filter(|r| *r > 0.0) };
        let Some(scale) = per(small) else {
            return eprintln!("poe.ninja: an exchange overview in {primary}, with no {small} rate");
        };
        if let Some(divine) = per("divine") {
            self.divine_rate = divine / scale;
        }
        for item in &ov.items {
            if let Some(details) = &item.details_id {
                self.exchange_history.insert(item.name.clone(), format!("exchange:{kind}:{details}"));
            }
        }
        let names: HashMap<_, _> = ov.items.into_iter().map(|i| (i.id, i.name)).collect();
        for line in ov.lines {
            if let (Some(name), Some(value)) = (names.get(&line.id), line.primary_value) {
                self.exchange.insert(name.clone(), value * scale);
                if let Some(spark) = line.sparkline {
                    self.exchange_spark.insert(name.clone(), spark);
                }
            }
        }
    }

    /// `kind`: the poe.ninja type the lines came from ("UniqueArmour", ...).
    /// `small`: the game's small unit, `chaos_value`'s unit.
    fn add_items(&mut self, kind: &str, mut ov: ItemOverview, small: &str) {
        // PoE2's lines are valued in the overview's primary currency
        // (divines) and word mods with markup ("[Resistances]").
        if let Some(core) = &ov.core {
            let primary = core.primary.as_deref().unwrap_or(small);
            let per = |c: &str| if c == primary { Some(1.0) } else { core.rates.get(c).copied().filter(|r| *r > 0.0) };
            let (small_per, divine_per) = (per(small), per("divine"));
            for line in &mut ov.lines {
                let Some(value) = line.primary_value else { continue };
                if let Some(s) = small_per {
                    line.chaos_value = value * s;
                }
                if let Some(d) = divine_per {
                    line.divine_value = Some(value * d);
                }
            }
        }
        for line in &mut ov.lines {
            line.kind = kind.to_string();
            for m in line
                .implicit_modifiers
                .iter_mut()
                .chain(&mut line.explicit_modifiers)
                .chain(&mut line.mutated_modifiers)
            {
                if m.text.contains('[') {
                    m.text = strip_markup(&m.text);
                }
            }
        }
        // Named by their base or enchant, so they get maps of their own:
        // `items` is looked up by name, and a rare "Vaal Regalia" mustn't
        // find the plain base's price there.
        let by_base = match kind {
            "BaseType" => Some(&mut self.bases),
            "ClusterJewel" => Some(&mut self.clusters),
            _ => None,
        };
        if kind == "ValdoMap" {
            for line in ov.lines {
                if let Some(reward) = line.variant.clone() {
                    self.valdo.entry(reward).or_default().push(line);
                }
            }
            return;
        }
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
                self.uniques_by_base.entry(unique_base(base).to_string()).or_default().push(line.clone());
            }
            self.items.entry(line.name.clone()).or_default().push(line);
        }
    }
}

/// The base `uniques_by_base` keys a unique by: a map's without its tier,
/// since poe.ninja lists a unique map at one tier ("Pillars of Arun" at
/// "Map (Tier 7)") and it drops at others, at the tier of its Replica
/// ("Map (Tier 16)"), which has the same art.
pub fn unique_base(base: &str) -> &str {
    match base.split_once(" (Tier ") {
        Some((map, tier)) if map.ends_with("Map") && tier.ends_with(')') => map,
        _ => base,
    }
}

/// PoE2 poe.ninja's mod markup: "[EnergyShield|Energy Shield]" -> "Energy
/// Shield", "[Resistances]" -> "Resistances".
fn strip_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find(']') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &rest[open + 1..open + close];
        out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn get<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
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
    let index: IndexState = get(&format!("{}/data/index-state", base()))?;
    Ok(index.economy_leagues.into_iter().map(|l| l.name).collect())
}

/// The current challenge league, falling back to Standard; or with
/// `hardcore`, its hardcore twin ("Hardcore Covens", PoE2's "HC Runes of
/// Aldur"), falling back to Hardcore.
pub fn pick_league(names: &[String], hardcore: bool) -> String {
    let (permanent, challenge): (&str, fn(&str) -> bool) = if hardcore {
        ("Hardcore", |n| n != "Hardcore" && is_hardcore(n) && !n.contains("Ruthless"))
    } else {
        ("Standard", |n| !is_hardcore(n) && !n.contains("Hardcore") && !n.contains("Ruthless") && n != "Standard")
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

    let small = game::current().small_unit();
    for kind in exchange_types() {
        let url = format!("{}/economy/exchange/current/overview?league={league_q}&type={kind}", base());
        match get::<ExchangeOverview>(&url) {
            Ok(ov) => db.add_exchange(kind, ov, small),
            Err(e) => {
                eprintln!("poe.ninja: {e}");
                failures += 1;
            }
        }
        thread::sleep(REQUEST_GAP);
    }

    for kind in item_types() {
        let url = format!("{}/economy/stash/current/item/overview?league={league_q}&type={kind}", base());
        match get::<ItemOverview>(&url) {
            Ok(ov) => db.add_items(kind, ov, small),
            Err(e) => {
                eprintln!("poe.ninja: {e}");
                failures += 1;
            }
        }
        thread::sleep(REQUEST_GAP);
    }

    if failures == exchange_types().len() + item_types().len() {
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
        ("UniqueMap", include_str!("../fixtures/ninja-unique-map.json")),
        ("SkillGem", include_str!("../fixtures/ninja-skill-gem.json")),
        ("Map", include_str!("../fixtures/ninja-map.json")),
        ("Viscera", include_str!("../fixtures/ninja-viscera.json")),
        ("ClusterJewel", include_str!("../fixtures/ninja-cluster-jewel.json")),
        ("BaseType", include_str!("../fixtures/ninja-base-type.json")),
        ("Wombgift", include_str!("../fixtures/ninja-wombgift.json")),
        ("ValdoMap", include_str!("../fixtures/ninja-valdo-map.json")),
        ("Corpse", include_str!("../fixtures/ninja-corpse.json")),
        ("BlightRavagedMap", include_str!("../fixtures/ninja-blight-ravaged-map.json")),
    ];

    /// A price list built from the fixtures the way `fetch` builds it.
    pub(crate) fn fixture_db() -> PriceDb {
        let mut db = PriceDb::new("Fixture");
        db.add_exchange("Currency", serde_json::from_str(EXCHANGE).expect("exchange overview"), "chaos");
        for (kind, json) in ITEMS {
            let ov: ItemOverview = serde_json::from_str(json).unwrap_or_else(|e| panic!("{kind} overview: {e}"));
            db.add_items(kind, ov, "chaos");
        }
        db
    }

    /// PoE2's price list from its fixtures, read as `fetch` reads them.
    pub(crate) fn poe2_fixture_db() -> PriceDb {
        let mut db = PriceDb::new("Fixture");
        let exchange = include_str!("../fixtures/poe2/ninja-exchange-currency.json");
        db.add_exchange("Currency", serde_json::from_str(exchange).unwrap(), "exalted");
        let armours = include_str!("../fixtures/poe2/ninja-unique-armours.json");
        db.add_items("UniqueArmours", serde_json::from_str(armours).unwrap(), "exalted");
        db
    }

    #[test]
    fn poe2_markup() {
        assert_eq!(
            strip_markup("+(100-150) to maximum [EnergyShield|Energy Shield]"),
            "+(100-150) to maximum Energy Shield"
        );
        assert_eq!(
            strip_markup("+(10-20)% to all [ElementalDamage|Elemental] [Resistances]"),
            "+(10-20)% to all Elemental Resistances"
        );
        assert_eq!(strip_markup("no markup"), "no markup");
    }

    /// A fixture line's week, as poe.ninja sent it.
    fn raw_spark(v: &serde_json::Value) -> Spark {
        Spark {
            total_change: v["totalChange"].as_f64().unwrap(),
            data: v["data"].as_array().unwrap().iter().map(serde_json::Value::as_f64).collect(),
        }
    }

    #[test]
    fn weeks() {
        let db = fixture_db();
        // Items: each line keeps its own.
        let raw: serde_json::Value = serde_json::from_str(ITEMS[0].1).unwrap();
        let first = &raw["lines"][0];
        let line = db.items[first["name"].as_str().unwrap()]
            .iter()
            .find(|l| l.variant.as_deref() == first["variant"].as_str() && l.links.is_none())
            .unwrap();
        assert_eq!(line.spark_line, Some(raw_spark(&first["sparkLine"])));
        // The exchange's, by name.
        let raw: serde_json::Value = serde_json::from_str(EXCHANGE).unwrap();
        let divine = raw["lines"].as_array().unwrap().iter().find(|l| l["id"] == "divine").unwrap();
        assert_eq!(db.exchange_spark["Divine Orb"], raw_spark(&divine["sparkline"]));
        assert_eq!(db.exchange_spark.len(), db.exchange.len());

        // PoE2: the same, and a day without listings stays empty.
        let db = poe2_fixture_db();
        assert!(!db.exchange_spark.is_empty());
        let weeks: Vec<&Spark> = db.items.values().flatten().filter_map(|l| l.spark_line.as_ref()).collect();
        assert!(!weeks.is_empty());
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/poe2/ninja-unique-armours.json")).unwrap();
        let gaps = raw["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["sparkLine"]["data"].as_array().unwrap().contains(&serde_json::Value::Null));
        if gaps {
            assert!(weeks.iter().any(|w| w.data.contains(&None)));
        }
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
    fn poe2_leagues() {
        let index: IndexState = serde_json::from_str(include_str!("../fixtures/poe2/ninja-index-state.json")).unwrap();
        let names: Vec<String> = index.economy_leagues.into_iter().map(|l| l.name).collect();
        let league = pick_league(&names, false);
        assert!(!is_hardcore(&league) && league != "Standard", "picked {league} from {names:?}");
        // Its hardcore twin is "HC <league>".
        assert_eq!(pick_league(&names, true), format!("HC {league}"), "from {names:?}");
        let only = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let names = only(&["Runes of Aldur", "HC Runes of Aldur", "Standard", "Hardcore"]);
        assert_eq!(pick_league(&names, false), "Runes of Aldur");
        assert_eq!(pick_league(&names, true), "HC Runes of Aldur");
    }

    /// PoE2's poe.ninja counts in divines: read as Exalted Orbs.
    #[test]
    fn poe2_exchange_in_exalted() {
        let json = include_str!("../fixtures/poe2/ninja-exchange-currency.json");
        let raw: serde_json::Value = serde_json::from_str(json).unwrap();
        let per_divine = raw["core"]["rates"]["exalted"].as_f64().unwrap();
        let mut db = PriceDb::new("Fixture");
        db.add_exchange("Currency", serde_json::from_str(json).unwrap(), "exalted");
        assert!((db.exchange["Exalted Orb"] - 1.0).abs() < 0.05, "an exalted is {}", db.exchange["Exalted Orb"]);
        assert!((db.exchange["Divine Orb"] - per_divine).abs() < 0.01 * per_divine);
        assert!((db.to_divine(db.exchange["Divine Orb"]) - 1.0).abs() < 0.01);
        assert!(db.exchange["Chaos Orb"] > db.exchange["Exalted Orb"]);
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
        let kept = [&db.items, &db.bases, &db.clusters, &db.valdo]
            .iter()
            .flat_map(|m| m.values())
            .map(Vec::len)
            .sum::<usize>();
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
