//! Official trade site search for rare/magic items, sent through the
//! headless browser (see cdp.rs for why).

use crate::cdp::{self, Browser, HttpResponse};
use crate::game;
use crate::item::{Eldritch, Item, ModKind, ModLine};
use crate::ninja::{self, PriceDb};
use crate::settings::min_fraction;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Stat/currency reference data is refetched after this long.
const DATA_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The browser is closed after this long without a search.
pub const BROWSER_IDLE: Duration = Duration::from_secs(10 * 60);
/// How long a search's or fetch's reply is reused for the same request
/// (Awakened PoE Trade's cache lifetime).
const REUSE_REPLIES: Duration = Duration::from_secs(300);
/// Listings fetched per search (one fetch request allows up to 10).
const LISTINGS: usize = 10;

/// What a filter searches: a stat (by trade id) or an item property such
/// as weapon DPS (by its group and key in the query's filters).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Stat(String),
    /// Stats the trade site words alike ("Regenerate # Energy Shield per
    /// second" has two ids), searched as a count group of at least one of
    /// them with the same range, like APT's merged stats. An item carries
    /// only one, so it's as exact as searching the right one.
    AnyStat(Vec<String>),
    /// A stat whose value is one of a list (a cluster jewel's grant, an
    /// anointed notable): its id and the option's number. The stat list
    /// names each option in full ("Allocates Charisma" is
    /// "enchant.stat_2954116742|6799").
    StatOption(String, u64),
    Property(&'static str, &'static str),
    /// A yes/no item filter, e.g. misc_filters.fractured_item.
    Flag(&'static str, &'static str),
    /// Items with none of these stats (a "not" group).
    NoneOf(Vec<String>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Weapon,
    Total,
    Mod,
    Implicit,
    /// Eldritch implicits, under the implicits: the layer colors them.
    Exarch,
    Eater,
    /// Implicits from corruption (a Vaal Orb), under the implicits: the
    /// layer colors them.
    Corruption,
    Enchant,
    /// A fractured mod, searched as fractured: always shown.
    Fractured,
    /// A crafted mod, searched as crafted: the layer colors it.
    Crafted,
    /// Base item properties: item level, fractured/synthesised.
    Base,
    /// An influence flag, by its trade key ("shaper"): the layer colors it.
    Influence(&'static str),
    /// Armour, evasion, energy shield, ward, block.
    Armour,
    /// A map's tier, quantity, rarity, pack size, blight.
    Map,
    /// A gem's level and quality.
    Gem,
}

impl Section {
    pub fn as_str(self) -> &'static str {
        match self {
            Section::Weapon => "weapon",
            Section::Total => "total",
            Section::Mod => "mod",
            Section::Implicit => "implicit",
            Section::Exarch => "exarch",
            Section::Eater => "eater",
            Section::Corruption => "corruption",
            Section::Enchant => "enchant",
            Section::Fractured => "fractured",
            Section::Crafted => "crafted",
            Section::Base => "base",
            Section::Influence(influence) => influence,
            Section::Armour => "armour",
            Section::Map => "map",
            Section::Gem => "gem",
        }
    }
}

pub struct Filter {
    pub label: String,
    pub target: Target,
    /// Default minimum: the roll minus 10%, kept within its tier range, or
    /// exactly the roll when it's the best the tier allows.
    pub min: Option<f64>,
    /// Range for the minimum's slider and wheel steps: the tier's roll range
    /// (summed for totals) when advanced copy gave it, else around the value.
    pub slider: Option<(f64, f64)>,
    /// The slider is drawn. Not for a mod or total without a roll range: a
    /// range made up around the value says nothing about the roll (its
    /// wheel still steps within it).
    pub draw_slider: bool,
    pub step: f64,
    /// The roll is at the top of its tier's range.
    pub max_roll: bool,
    /// Ticked by default (like Awakened PoE Trade: total life, the
    /// resistance totals and the main weapon DPS; everything else unticked).
    pub default_on: bool,
    pub section: Section,
    /// The mod's affix tier (1 is best), for listed mods from advanced copy.
    pub tier: Option<u32>,
    /// How much quality raises the mod (a catalyst), from advanced copy. The
    /// value and range already include it.
    pub increased: Option<u32>,
    /// The item's own value, marked on the slider.
    pub roll: Option<f64>,
    /// Default maximum. Only Viscera have one: their tiers share a trade
    /// base, so a limb's implicit is capped at its tier's best roll.
    pub max: Option<f64>,
}

impl Filter {
    /// `value`: the item's own (average for "Adds # to #", sums for totals).
    fn new(
        label: String,
        target: Target,
        value: f64,
        bounds: Option<(f64, f64)>,
        on: bool,
        section: Section,
    ) -> Filter {
        let (min, slider, step, max_roll) = defaults(value, bounds, false);
        Filter {
            // A mod the game wraps onto two lines reads as one.
            label: label.replace('\n', " "),
            target,
            min,
            slider,
            draw_slider: true,
            step,
            max_roll,
            default_on: on,
            section,
            tier: None,
            increased: None,
            roll: Some(value),
            max: None,
        }
    }

    /// A unique's rolled mod, unticked. Like APT, the minimum is the roll
    /// minus 10% of the mod's range, kept within the range (a perfect roll:
    /// exactly the roll).
    fn unique(label: String, target: Target, value: f64, (lo, hi): (f64, f64), section: Section) -> Filter {
        let mut f = Filter::new(label, target, value, Some((lo, hi)), false, section);
        if let (Some(_), false) = (f.min, f.max_roll) {
            let floor = lo.min(value);
            let min =
                ((round_down(value - (hi - lo) * (1.0 - min_fraction()), f.step)) / f.step).round() / (1.0 / f.step);
            f.min = Some(min.max(floor));
            f.slider = Some((floor, hi));
        }
        f
    }
}

/// A value and the range it can roll in, summed over the mods it comes from.
#[derive(Clone, Copy, Default)]
struct Amount {
    value: f64,
    lo: f64,
    hi: f64,
    unbounded: bool,
    seen: bool,
}

impl Amount {
    fn add(&mut self, value: f64, range: Option<(f64, f64)>, factor: f64) {
        self.seen = true;
        self.value += value * factor;
        match range {
            Some((lo, hi)) => {
                self.lo += lo * factor;
                self.hi += hi * factor;
            }
            None => self.unbounded = true,
        }
    }
    fn bounds(&self) -> Option<(f64, f64)> {
        (!self.unbounded && self.hi > 0.0).then_some((self.lo, self.hi))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Corrupted {
    Any,
    No,
    Yes,
}

impl Corrupted {
    pub fn as_str(self) -> &'static str {
        match self {
            Corrupted::Any => "any",
            Corrupted::No => "no",
            Corrupted::Yes => "yes",
        }
    }
    pub fn parse(s: &str) -> Corrupted {
        match s {
            "no" => Corrupted::No,
            "yes" => Corrupted::Yes,
            _ => Corrupted::Any,
        }
    }
}

/// Which listings to search: the trade site's "status" option.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Status {
    /// Merchant tabs only ("securable"): bought on the spot, so a seller
    /// can't list low and refuse to sell; the default, like APT's.
    #[default]
    Instant,
    /// Instant buyout and sellers online in person ("available"; the
    /// API's "online" alone would leave merchant tabs out).
    Available,
    /// Every listing.
    Any,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Instant => "instant",
            Status::Available => "available",
            Status::Any => "any",
        }
    }
    pub fn parse(s: &str) -> Status {
        match s {
            "available" => Status::Available,
            "any" => Status::Any,
            _ => Status::Instant,
        }
    }
    /// The trade API's name for it.
    fn option(self) -> &'static str {
        match self {
            Status::Instant => "securable",
            Status::Available => "available",
            Status::Any => "any",
        }
    }
}

/// How to search, besides the stat filters.
#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    /// The exact base type ("Sapphire Ring") rather than its category.
    pub base_only: bool,
    pub corrupted: Corrupted,
    pub preset: Preset,
    pub status: Status,
    /// Only 6-linked items. Offered for a 6-link (and on by default): links
    /// are a toggle, not a filter, since fewer than six are worth little.
    pub six_link: bool,
    /// Only unidentified items (offered for an unidentified one; else any).
    /// Some uniques sell for a gamble on their rolls, Watcher's Eye say.
    pub unidentified: bool,
}

/// Which filters an item gets, like APT's presets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    /// Totals and mods (APT's "pseudo" preset).
    Normal,
    /// The item as a crafting base: exact base, item level, influences,
    /// fractured/synthesised, fractured mods, enchants and implicits.
    Base,
}

impl Preset {
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Normal => "normal",
            Preset::Base => "base",
        }
    }
    pub fn parse(s: &str) -> Preset {
        if s == "base" { Preset::Base } else { Preset::Normal }
    }
}

/// Like APT: the "base item" preset is offered for unfinished gear with
/// crafting value (fractured, synthesised, influenced, cluster or magic
/// jewels, or item level 82+).
/// Also offered for any white item and any 5- or 6-link, which sell as a
/// (linked) base.
pub fn base_preset_available(item: &Item) -> bool {
    let class = item.class.as_str();
    let gear = category(item).is_some()
        && !matches!(
            class,
            "Maps" | "Flasks" | "Tinctures" | "Life Flasks" | "Mana Flasks" | "Hybrid Flasks" | "Utility Flasks"
        );
    if !gear {
        return false;
    }
    // A white item is only its base (and implicits, sockets, quality).
    if item.rarity == "Normal" || (item.links >= 5 && matches!(item.rarity.as_str(), "Magic" | "Rare")) {
        return true;
    }
    let finished =
        item.corrupted || item.mirrored || item.quality == 20 || item.mods.iter().any(|m| m.kind == ModKind::Crafted);
    matches!(item.rarity.as_str(), "Magic" | "Rare") && !item.mods.is_empty() && !finished && crafting_value(item)
}

/// Fractured, synthesised, influenced, cluster or magic jewels, item level 82+.
fn crafting_value(item: &Item) -> bool {
    let jewel = item.class.contains("Jewel");
    item.fractured_item
        || item.synthesised
        || !item.influences.is_empty()
        || item.base.contains("Cluster Jewel")
        || (jewel && item.rarity == "Magic")
        || (!jewel && item.item_level.unwrap_or(0) >= 82)
}

/// The preset a price check starts with: a white or magic 5/6-link is
/// worth its links and base, not its mods; a white item without mods has
/// nothing but its base to compare.
pub fn default_preset(item: &Item) -> Preset {
    let linked = item.links >= 5 && matches!(item.rarity.as_str(), "Normal" | "Magic");
    let plain = item.rarity == "Normal" && item.mods.is_empty();
    if (linked || plain) && base_preset_available(item) { Preset::Base } else { Preset::Normal }
}

/// What the panel shows before anything is searched.
pub struct Prepared {
    pub filters: Vec<Filter>,
    /// "Ring" for the "Any Ring" / "Sapphire Ring" toggle; `None` when the
    /// base can't be searched separately (unknown category, or a magic item,
    /// whose name hides the base among its affixes).
    pub category_label: Option<&'static str>,
    /// The "base item" preset can be offered.
    pub base_preset: bool,
    pub defaults: SearchOptions,
}

/// Listings at the same price, grouped (they're sorted by price).
pub struct Listing {
    pub amount: f64,
    pub currency: String,
    /// The trade site's currency id ("alch"), for its icon.
    pub currency_id: String,
    pub chaos: Option<f64>,
    pub count: usize,
    /// How long ago they were listed: "5d", or a range "41h–5d".
    pub age: String,
    /// The first of them.
    pub preview: Preview,
    /// What they have of the search's enabled filters (the same for all
    /// of them: only listings alike in these are grouped).
    pub values: Values,
}

/// A one-line answer: what the item would sell for (see `estimate_index`).
pub struct Summary {
    /// The price is set by a listing that has sat unsold (see `STALE_MINUTES`),
    /// so the item sells for less than this.
    pub capped: bool,
    pub amount: f64,
    pub currency: String,
    pub currency_id: String,
    pub chaos: Option<f64>,
    /// "1–2 Orb of Alchemy", or in chaos when currencies differ.
    pub range: String,
}

pub struct TradeResult {
    pub listings: Vec<Listing>,
    pub summary: Option<Summary>,
    pub total: u64,
    pub url: String,
}

struct TradeData {
    /// (stat group, text template) -> trade stat id: the first listed of
    /// those with that text.
    stats: HashMap<(String, String), String>,
    /// (stat group, text template) -> every id with that text, in the trade
    /// site's order, for texts with more than one.
    same_text: HashMap<(String, String), Vec<String>>,
    /// (stat group, text with every number made `#`) -> each stat text with
    /// fixed numbers that reads so, and its `number_slots`.
    numbered: HashMap<(String, String), Vec<(String, Slots)>>,
    /// currency id ("divine") -> name ("Divine Orb")
    currencies: HashMap<String, String>,
    /// currency id -> icon path on web.poecdn.com
    images: HashMap<String, String>,
    /// Transfigured gems: name ("Arc of Oscillating") -> the base gem
    /// ("Arc") and the trade site's discriminator ("alt_y").
    transfigured: HashMap<String, (String, String)>,
    /// Mercenary Warrants: build ("Chaplain of the Well") -> its type
    /// ("AbyssalTemplar") and discriminator ("mercenary_warrant").
    warrants: HashMap<String, (String, String)>,
    /// Every item the trade site knows by name, for reading names off the
    /// screen.
    names: Arc<Vec<NamedItem>>,
    /// (stat group, lowercase text template) -> the template as the trade
    /// site writes it: text read off the screen comes in small caps.
    lower_stats: HashMap<(String, String), String>,
}

/// An item name from the trade site's item list (/api/trade/data/items).
#[derive(Debug, Clone, PartialEq)]
pub struct NamedItem {
    /// A unique's name; `None` for everything else.
    pub name: Option<String>,
    /// The base type, a gem's name (a transfigured one's own), a currency's.
    pub base: String,
    /// The list's group: "accessory", "armour", "currency", "gem", "card"...
    pub group: String,
}

/// Weapon-local stats the trade site lists with the same text as the global
/// one, without "(Local)": impale and intimidate on hit. Its order of the two
/// differs between groups, so they're told apart by id (APT's data has the
/// same pairs). Read as "(Local)" stats.
const UNLABELLED_LOCAL: [&str; 2] = ["stat_725880290", "stat_2089652545"];

/// Tincture stats the trade site words like a flask's ("#% increased
/// effect") or a jewel's ("#% increased Cooldown Recovery Rate"). A
/// tincture's mod is searched as these, anything else's never (APT's
/// "select" stat groups).
const TINCTURE_STATS: [&str; 2] = ["stat_3529940209", "stat_239144"];

impl TradeData {
    /// From /api/trade/data/stats, /api/trade/data/static and
    /// /api/trade/data/items.
    fn from_json(stats: &Value, statics: &Value, items: &Value) -> TradeData {
        let mut data = TradeData {
            stats: HashMap::new(),
            same_text: HashMap::new(),
            numbered: HashMap::new(),
            currencies: HashMap::new(),
            images: HashMap::new(),
            transfigured: HashMap::new(),
            warrants: HashMap::new(),
            names: Arc::new(Vec::new()),
            lower_stats: HashMap::new(),
        };
        let mut names = Vec::new();
        for group in items["result"].as_array().into_iter().flatten() {
            let group_id = group["id"].as_str().unwrap_or("");
            for e in group["entries"].as_array().into_iter().flatten() {
                let Some(base) = e["type"].as_str() else { continue };
                let unique = e["flags"]["unique"].as_bool() == Some(true);
                let (name, base) = match (unique, e["name"].as_str(), e["text"].as_str()) {
                    (true, Some(name), _) => (Some(name.to_string()), base.to_string()),
                    // A transfigured gem's own name.
                    (false, _, Some(text)) if group_id == "gem" => (None, text.to_string()),
                    _ => (None, base.to_string()),
                };
                names.push(NamedItem { name, base, group: group_id.to_string() });
            }
        }
        data.names = Arc::new(names);
        for group in items["result"].as_array().into_iter().flatten().filter(|g| g["id"] == "gem") {
            for e in group["entries"].as_array().into_iter().flatten() {
                if let (Some(base), Some(text), Some(disc)) =
                    (e["type"].as_str(), e["text"].as_str(), e["disc"].as_str())
                {
                    data.transfigured.insert(text.to_string(), (base.to_string(), disc.to_string()));
                }
            }
        }
        for e in
            items["result"].as_array().into_iter().flatten().flat_map(|g| g["entries"].as_array().into_iter().flatten())
        {
            if let (Some(base), Some(text), Some(disc @ "mercenary_warrant")) =
                (e["type"].as_str(), e["text"].as_str(), e["disc"].as_str())
                && let Some(build) = text.strip_prefix("Mercenary Warrant (").and_then(|t| t.strip_suffix(')'))
            {
                data.warrants.insert(build.to_string(), (base.to_string(), disc.to_string()));
            }
        }
        for group in stats["result"].as_array().into_iter().flatten() {
            let group_id = group["id"].as_str().unwrap_or("");
            for e in group["entries"].as_array().into_iter().flatten() {
                if let (Some(id), Some(text)) = (e["id"].as_str(), e["text"].as_str()) {
                    let text = match id.rsplit_once('.') {
                        Some((_, stat)) if UNLABELLED_LOCAL.contains(&stat) => format!("{text} (Local)"),
                        _ => text.to_string(),
                    };
                    let key = (group_id.to_string(), text);
                    data.lower_stats.entry((key.0.clone(), key.1.to_lowercase())).or_insert_with(|| key.1.clone());
                    match data.stats.get(&key) {
                        None => {
                            let (template, slots) = number_slots(&key.1);
                            if slots.iter().any(Option::is_some) {
                                let numbered = data.numbered.entry((key.0.clone(), template)).or_default();
                                numbered.push((key.1.clone(), slots));
                            }
                            data.stats.insert(key, id.to_string());
                        }
                        Some(first) if first != id => {
                            let first = first.clone();
                            data.same_text.entry(key).or_insert_with(|| vec![first]).push(id.to_string());
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        for group in statics["result"].as_array().into_iter().flatten() {
            for e in group["entries"].as_array().into_iter().flatten() {
                if let (Some(id), Some(text)) = (e["id"].as_str(), e["text"].as_str()) {
                    data.currencies.insert(id.to_string(), text.to_string());
                    if let Some(image) = e["image"].as_str() {
                        data.images.insert(id.to_string(), image.to_string());
                    }
                }
            }
        }
        data
    }

    /// See Trade::canonical_mod.
    fn canonical_mod(&self, line: &str) -> Option<(ModKind, String)> {
        let (template, values) = templatize(line.trim());
        let lower = template.to_lowercase();
        for (group, kind) in
            [("explicit", ModKind::Explicit), ("implicit", ModKind::Implicit), ("enchant", ModKind::Enchant)]
        {
            let Some(text) = self.lower_stats.get(&(group.to_string(), lower.clone())) else { continue };
            let mut values = values.iter();
            let line = text
                .split('#')
                .enumerate()
                .map(|(i, part)| {
                    if i == 0 {
                        part.to_string()
                    } else {
                        format!("{}{part}", values.next().map_or("#".into(), |v| trim_number(*v)))
                    }
                })
                .collect();
            return Some((kind, line));
        }
        None
    }

    /// Every id with a text in a group, the first listed first.
    fn stat_ids(&self, group: &str, text: &str) -> Option<Vec<String>> {
        let key = (group.to_string(), text.to_string());
        self.same_text.get(&key).cloned().or_else(|| Some(vec![self.stats.get(&key)?.clone()]))
    }
}

pub struct Trade {
    browser: Option<Browser>,
    data: Option<(TradeData, Instant)>,
    limiter: RateLimiter,
    last_used: Instant,
    /// Recent search and fetch replies, by request (see `request`).
    recent: HashMap<String, (Instant, HttpResponse)>,
}

/// Items worth a trade search: anything non-unique with mods, white gear
/// (searched as a base), and white maps and invitations (by type).
pub fn searchable(item: &Item) -> bool {
    match item.rarity.as_str() {
        "Rare" | "Magic" => !item.mods.is_empty() || item.links >= 5,
        "Normal" => {
            item.warrant.is_some()
                || !item.mods.is_empty()
                || base_preset_available(item)
                || crate::mapcheck::is_map_like(item)
        }
        // Identified uniques: searched by name, with their rolls. Also
        // unidentified ones once their art says which unique they are.
        "Unique" => item.name.is_some() && (!item.mods.is_empty() || item.unidentified),
        // Gems by name, level and quality.
        "Gem" => true,
        // Mercenary Warrants: by build, skills and supports.
        _ if item.warrant.is_some() => true,
        // Facetor's Lens: by its stored experience.
        _ if item.stored_experience.is_some() => true,
        // Wickerman limbs have no rarity: searched by base and implicits.
        _ => item.class == "Viscera" && !item.mods.is_empty(),
    }
}

/// The base type the trade site knows. Viscera tiers share one:
/// "Lesser Brute's Arm" and "Greater Brute's Arm" are both "Brute's Arm".
fn trade_base(item: &Item) -> &str {
    if item.class == "Viscera" {
        for tier in ["Lesser ", "Greater "] {
            if let Some(base) = item.base.strip_prefix(tier) {
                return base;
            }
        }
    }
    &item.base
}

/// An item is compared like for like: corrupted, it can't be crafted any
/// more (and may have another implicit), so craftable listings oversell it,
/// and corrupted ones undersell an uncorrupted item. The panel can widen
/// this to any.
fn default_corrupted(item: &Item) -> Corrupted {
    if item.corrupted { Corrupted::Yes } else { Corrupted::No }
}

impl Trade {
    pub fn new() -> Trade {
        Trade {
            browser: None,
            data: None,
            limiter: RateLimiter::default(),
            last_used: Instant::now(),
            recent: HashMap::new(),
        }
    }

    pub fn close_if_idle(&mut self) {
        if self.browser.is_some() && self.last_used.elapsed() > BROWSER_IDLE {
            eprintln!("trade: closing idle browser");
            self.browser = None;
        }
    }

    /// Every item name the trade site knows (cached on disk for a day).
    pub fn item_names(&mut self) -> Result<Arc<Vec<NamedItem>>, String> {
        self.ensure_data()?;
        Ok(self.data.as_ref().unwrap().0.names.clone())
    }

    /// A mod line read off the screen ("+23% TO COLD RESISTANCE"), as the
    /// game writes it ("+23% to Cold Resistance"), with its kind, if the
    /// trade site has the stat as an explicit, implicit or enchant (Heist's
    /// "#% increased Explicit Chaos Modifier magnitudes").
    pub fn canonical_mod(&mut self, line: &str) -> Option<(ModKind, String)> {
        self.ensure_data().ok()?;
        self.data.as_ref().unwrap().0.canonical_mod(line)
    }

    /// The filters and options for an item, without searching. Needs only
    /// the stat list (cached on disk for a day), so it normally makes no
    /// requests at all.
    pub fn prepare(&mut self, item: &Item, preset: Preset) -> Result<Prepared, String> {
        self.ensure_data()?;
        let data = &self.data.as_ref().unwrap().0;
        let filters = build_filters(item, data, preset);
        // Magic items' base is only known from advanced copy's affix names.
        let base_known = item.rarity != "Magic" || item.name.is_some();
        let category_label = match (category(item), item.rarity.as_str()) {
            (Some((_, label)), "Rare" | "Normal" | "Magic") if base_known => Some(label),
            _ => None,
        };
        let corrupted = default_corrupted(item);
        // A map's base names its tier ("Map (Tier 16)"): search that base.
        let base_only = preset == Preset::Base || tiered_map_base(item);
        Ok(Prepared {
            filters,
            category_label,
            base_preset: base_preset_available(item),
            defaults: SearchOptions {
                base_only,
                corrupted,
                preset,
                status: crate::settings::default_status(),
                six_link: item.links >= 6,
                unidentified: false,
            },
        })
    }

    /// `enabled`: the prepared filters to use, by index, with their bounds.
    pub fn search(
        &mut self,
        item: &Item,
        enabled: &[Choice],
        options: SearchOptions,
        league: &str,
        prices: &PriceDb,
    ) -> Result<TradeResult, String> {
        self.last_used = Instant::now();
        let result = self.search_inner(item, enabled, options, league, prices);
        // A dead or confused browser gets relaunched next time.
        if result.is_err() && !self.browser.as_mut().is_some_and(|b| b.alive()) {
            self.browser = None;
        }
        result
    }

    fn search_inner(
        &mut self,
        item: &Item,
        enabled: &[Choice],
        options: SearchOptions,
        league: &str,
        prices: &PriceDb,
    ) -> Result<TradeResult, String> {
        self.ensure_data()?;
        let (filters, chosen, body) = search_body(&self.data.as_ref().unwrap().0, item, enabled, options);
        let body = body.to_string();
        let league_path = ninja::encode(league);

        let resp = self.request(
            "search",
            "POST",
            &format!("{}/search/{league_path}", game::current().trade_api()),
            Some(&body),
        )?;
        let (id, total, hashes) = parse_search(&resp.body)?;

        // One entry per listing first, with what each has of the enabled
        // filters. Totals come only when the fetch names them.
        let columns: Vec<(usize, &Target)> =
            filters.iter().enumerate().filter(|(i, _)| chosen[*i].is_some()).map(|(i, f)| (i, &f.target)).collect();
        let pseudos: String = columns
            .iter()
            .filter_map(|(_, t)| match t {
                Target::Stat(id) if id.starts_with("pseudo.") => Some(format!("&pseudos[]={id}")),
                _ => None,
            })
            .collect();
        let mut singles: Vec<Single> = Vec::new();
        if !hashes.is_empty() {
            let path = format!("{}/fetch/{}?query={id}{pseudos}", game::current().trade_api(), hashes.join(","));
            let resp = self.request("fetch", "GET", &path, None)?;
            let data = &self.data.as_ref().unwrap().0;
            singles = parse_fetch(&resp.body, data, prices, unix_now(), &columns)?;
        }
        Ok(trade_result(singles, total, &id, &league_path))
    }

    /// The search for these filters on the trade site, without searching:
    /// the query in the page's address ("?q="), so it needs no request to
    /// the trade API and follows the filters as they are now.
    pub fn site_url(
        &mut self,
        item: &Item,
        enabled: &[Choice],
        options: SearchOptions,
        league: &str,
    ) -> Result<String, String> {
        self.ensure_data()?;
        let (_, _, body) = search_body(&self.data.as_ref().unwrap().0, item, enabled, options);
        Ok(site_url(&body, league))
    }

    /// A currency's icon (PNG), from web.poecdn.com (a plain CDN, no browser
    /// needed), cached on disk.
    /// Also a listed item's image, by its URL (`Preview`'s "icon"), cached
    /// under a hash of it, and `SOCKET_SPRITE`: the trade site's sheet of
    /// socket and link images.
    pub fn icon(&mut self, currency_id: &str) -> Result<Vec<u8>, String> {
        let is_url = currency_id.starts_with(ICON_CDN);
        if currency_id.is_empty()
            || !is_url && !currency_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("bad currency id".into());
        }
        let name = if is_url { item_image_name(currency_id) } else { currency_id.to_string() };
        let path = cache_dir().join("icons").join(format!("{name}.png"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(bytes);
        }
        let url = if is_url {
            currency_id.to_string()
        } else if currency_id == SOCKET_SPRITE {
            socket_sprite_url()?
        } else {
            self.ensure_data()?;
            let data = &self.data.as_ref().unwrap().0;
            let image = data.images.get(currency_id).ok_or("no icon for this currency")?;
            format!("https://web.poecdn.com{image}")
        };
        download_image(&url, &path)
    }

    fn browser(&mut self) -> Result<&mut Browser, String> {
        if !self.browser.as_mut().is_some_and(|b| b.alive()) {
            let binary = cdp::find_binary().ok_or("Trade search needs Chromium or Chrome installed")?;
            eprintln!("trade: starting {binary}");
            self.browser = Some(Browser::launch(&binary, &cache_dir().join("chromium"))?);
        }
        Ok(self.browser.as_mut().unwrap())
    }

    /// A rate-limited request; non-2xx replies become errors.
    fn request(
        &mut self,
        endpoint: &'static str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<HttpResponse, String> {
        // The same search or fetch again soon (Search pressed twice, a
        // check repeated) is answered from what came back, like APT does.
        let cached = matches!(endpoint, "search" | "fetch");
        let key = format!("{method} {path} {}", body.unwrap_or(""));
        if cached
            && let Some((at, resp)) = self.recent.get(&key)
            && at.elapsed() < REUSE_REPLIES
        {
            return Ok(resp.clone());
        }
        let resp = self.request_uncached(endpoint, method, path, body)?;
        if cached {
            self.recent.retain(|_, (at, _)| at.elapsed() < REUSE_REPLIES);
            self.recent.insert(key, (Instant::now(), resp.clone()));
        }
        Ok(resp)
    }

    fn request_uncached(
        &mut self,
        endpoint: &'static str,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<HttpResponse, String> {
        self.limiter.check(endpoint)?;
        let resp = match self.browser()?.fetch(method, path, body) {
            Ok(resp) => resp,
            // The browser went away since the last request (killed, or
            // crashed): start a new one and try once more.
            Err(e) if e == "chromium exited" || e.starts_with("chromium pipe") => {
                eprintln!("trade: {e}; restarting the browser");
                self.browser = None;
                self.browser()?.fetch(method, path, body)?
            }
            Err(e) => return Err(e),
        };
        self.limiter.record(endpoint, &resp);
        if resp.status == 200 {
            return Ok(resp);
        }
        if resp.body.trim_start().starts_with('<') {
            return Err(format!("pathofexile.com refused the request (HTTP {}, Cloudflare?)", resp.status));
        }
        let message = serde_json::from_str::<Value>(&resp.body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| "no details".into());
        Err(format!("Trade API error {}: {message}", resp.status))
    }

    fn ensure_data(&mut self) -> Result<(), String> {
        if self.data.as_ref().is_some_and(|(_, at)| at.elapsed() < DATA_MAX_AGE) {
            return Ok(());
        }
        let stats = self.cached_json("stats")?;
        let statics = self.cached_json("static")?;
        // Only for transfigured gems: carry on without it.
        let items = self.cached_json("items").unwrap_or_else(|e| {
            eprintln!("trade: item list: {e}");
            Value::Null
        });

        let data = TradeData::from_json(&stats, &statics, &items);
        eprintln!("trade: {} stat templates, {} currencies", data.stats.len(), data.currencies.len());
        self.data = Some((data, Instant::now()));
        Ok(())
    }

    /// /api/trade/data/<name>, from the disk cache when fresh.
    fn cached_json(&mut self, name: &'static str) -> Result<Value, String> {
        let path = cache_dir().join(format!("trade-{name}.json"));
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or(Duration::MAX) < DATA_MAX_AGE);
        if fresh && let Some(v) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
            return Ok(v);
        }
        let resp = self.request("data", "GET", &format!("{}/data/{name}", game::current().trade_api()), None)?;
        let v: Value = serde_json::from_str(&resp.body).map_err(|e| format!("bad {name} data: {e}"))?;
        let _ = std::fs::create_dir_all(cache_dir());
        let _ = std::fs::write(&path, &resp.body);
        Ok(v)
    }
}

/// The search reply: query id, total matches, the first listings' hashes.
fn parse_search(body: &str) -> Result<(String, u64, Vec<String>), String> {
    let found: Value = serde_json::from_str(body).map_err(|e| format!("bad search reply: {e}"))?;
    let id = found["id"].as_str().ok_or("search reply has no id")?.to_string();
    let total = found["total"].as_u64().unwrap_or(0);
    let hashes = found["result"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).take(LISTINGS).map(str::to_string).collect())
        .unwrap_or_default();
    Ok((id, total, hashes))
}

/// The fetch reply: one entry per priced listing, in the reply's order.
/// `now`: seconds since the epoch, for the listings' ages. `columns`: the
/// enabled filters (by index), whose values each listing's item is read for.
fn parse_fetch(
    body: &str,
    data: &TradeData,
    prices: &PriceDb,
    now: i64,
    columns: &[(usize, &Target)],
) -> Result<Vec<Single>, String> {
    let fetched: Value = serde_json::from_str(body).map_err(|e| format!("bad fetch reply: {e}"))?;
    let mut singles = Vec::new();
    for entry in fetched["result"].as_array().into_iter().flatten() {
        let listing = &entry["listing"];
        let (Some(amount), Some(currency)) =
            (listing["price"]["amount"].as_f64(), listing["price"]["currency"].as_str())
        else {
            continue; // unpriced
        };
        let name = data.currencies.get(currency).cloned().unwrap_or_else(|| currency.to_string());
        let chaos = if currency == game::current().small_unit() {
            Some(amount)
        } else {
            prices.exchange.get(&name).map(|c| c * amount)
        };
        let minutes = listing["indexed"].as_str().and_then(parse_utc).map(|then| (now - then).max(0) / 60);
        let item = &entry["item"];
        let values = columns.iter().filter_map(|(i, t)| listing_value(item, t).map(|v| (*i, v))).collect();
        singles.push((amount, currency.to_string(), name, chaos, minutes, item_preview(item), values));
    }
    Ok(singles)
}

fn gem_line(preview: &Preview) -> Option<&str> {
    preview.iter().find(|l| l.0 == "gem").map(|l| l.1.as_str())
}

/// Listings at the same price grouped, with the estimate.
fn trade_result(singles: Vec<Single>, total: u64, id: &str, league_path: &str) -> TradeResult {
    let summary = estimate_index(&singles).map(|(i, capped)| {
        let (amount, id, name, chaos, _, _, _) = &singles[i];
        let same_currency = singles.iter().all(|s| s.1 == *id);
        let (first, last) = (&singles[0], &singles[singles.len() - 1]);
        let range = if same_currency {
            format!("{}–{} {}", trim_number(first.0), trim_number(last.0), name)
        } else {
            let c = |s: &Single| s.3.map(trim_number).unwrap_or("?".into());
            format!("{}–{}{}", c(first), c(last), game::current().small_suffix())
        };
        Summary { capped, amount: *amount, currency: name.clone(), currency_id: id.clone(), chaos: *chaos, range }
    });

    let mut listings: Vec<Listing> = Vec::new();
    let mut ages: Vec<Vec<i64>> = Vec::new();
    for (amount, id, name, chaos, minutes, preview, values) in singles {
        match listings.last_mut() {
            // Gems at one price but different levels or quality stay apart,
            // and so do items with different values in the filters' columns.
            Some(last)
                if last.amount == amount
                    && last.currency_id == id
                    && gem_line(&last.preview) == gem_line(&preview)
                    && last.values == values =>
            {
                last.count += 1;
                ages.last_mut().unwrap().extend(minutes);
            }
            _ => {
                listings.push(Listing {
                    amount,
                    currency: name,
                    currency_id: id,
                    chaos,
                    count: 1,
                    age: String::new(),
                    preview,
                    values,
                });
                ages.push(minutes.into_iter().collect());
            }
        }
    }
    for (listing, ages) in listings.iter_mut().zip(ages) {
        listing.age = match (ages.iter().min(), ages.iter().max()) {
            (Some(&lo), Some(&hi)) if format_age(lo) != format_age(hi) => {
                format!("{}–{}", format_age(lo), format_age(hi))
            }
            (Some(&lo), _) => format_age(lo),
            _ => String::new(),
        };
    }

    TradeResult { listings, summary, total, url: format!("{}/{league_path}/{id}", game::current().trade_site()) }
}

/// An item's image on web.poecdn.com (a listed item's, or a unique's art
/// from poe.ninja), cached on disk like `Trade::icon`'s. Needs no `Trade`,
/// so fetching art doesn't wait for a trade search.
pub fn item_image(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with(ICON_CDN) || url.contains(char::is_whitespace) {
        return Err("not a web.poecdn.com URL".into());
    }
    let path = item_image_path(url);
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(bytes);
    }
    // One download per URL: a second request (the art prefetch and a match
    // wanting the same image) waits for it and reads the cache.
    {
        let mut busy = DOWNLOADING.lock().unwrap();
        while busy.iter().any(|u| u == url) {
            busy = DOWNLOADED.wait(busy).unwrap();
        }
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(bytes);
        }
        busy.push(url.to_string());
    }
    let result = download_image(url, &path);
    DOWNLOADING.lock().unwrap().retain(|u| u != url);
    DOWNLOADED.notify_all();
    result
}

/// Whether `item_image` has this one on disk already.
pub fn item_image_cached(url: &str) -> bool {
    item_image_path(url).exists()
}

static DOWNLOADING: Mutex<Vec<String>> = Mutex::new(Vec::new());
static DOWNLOADED: Condvar = Condvar::new();

fn item_image_path(url: &str) -> PathBuf {
    cache_dir().join("icons").join(format!("{}.png", item_image_name(url)))
}

fn item_image_name(url: &str) -> String {
    format!("item-{:016x}", fnv1a(url))
}

fn download_image(url: &str, path: &std::path::Path) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .set("User-Agent", ninja::USER_AGENT)
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader().take(1 << 20).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    // Written whole, then renamed: another thread may be reading the cache.
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let part = path.with_extension("part");
    if std::fs::write(&part, &bytes).is_ok() {
        let _ = std::fs::rename(&part, path);
    }
    Ok(bytes)
}

pub(crate) fn cache_dir() -> PathBuf {
    game::cache_dir()
}

/// A listing this old has had time to sell and hasn't: it's overpriced.
const STALE_MINUTES: i64 = 24 * 60;

/// Which listing (sorted cheapest first) prices the item, and whether it's a
/// stale one, which caps the price rather than meeting it.
///
/// Buyers take the cheapest offers, so the expensive end of the list says
/// little: the estimate is the middle of the cheapest quarter (a single
/// lowball doesn't set it). And nothing above a listing that's sat unsold
/// for a day is going to sell, so the cheapest stale listing caps it.
fn estimate_index(singles: &[Single]) -> Option<(usize, bool)> {
    if singles.is_empty() {
        return None;
    }
    let low = (singles.len() / 4).max(1);
    let pick = low / 2;
    let stale = singles.iter().position(|s| s.4.is_some_and(|m| m >= STALE_MINUTES));
    Some(match stale {
        Some(s) if s <= pick => (s, true),
        _ => (pick, false),
    })
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Minutes as "2m" / "3h" / "5d".
fn format_age(minutes: i64) -> String {
    match minutes {
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h", m / 60),
        m => format!("{}d", m / (24 * 60)),
    }
}

/// 1.0 -> "1", 0.25 -> "0.25", 12.5 -> "12.5".
pub fn trim_number(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Seconds since the epoch for an ISO 8601 UTC timestamp.
fn parse_utc(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

// ---- mods -> trade filters -------------------------------------------------

/// "+45 to maximum Life" -> ("+# to maximum Life", [45.0]).
pub(crate) fn templatize(line: &str) -> (String, Vec<f64>) {
    let mut template = String::new();
    let mut values = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if !c.is_ascii_digit() {
            template.push(c);
            continue;
        }
        let mut num = c.to_string();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() || (d == '.' && !num.contains('.')) {
                num.push(d);
                chars.next();
            } else {
                break;
            }
        }
        values.push(num.trim_end_matches('.').parse().unwrap_or(0.0));
        template.push('#');
    }
    (template, values)
}

/// "+#% to Fire and Cold Resistances" -> Some((2 elements, no chaos)).
fn resist_parts(template: &str) -> Option<(u32, bool)> {
    let t = template.strip_prefix("+#% to ")?;
    let t = t.strip_suffix(" Resistances").or_else(|| t.strip_suffix(" Resistance"))?;
    if t == "all Elemental" {
        return Some((3, false));
    }
    let (mut elements, mut chaos) = (0, false);
    for word in t.split(" and ") {
        match word {
            "Fire" | "Cold" | "Lightning" => elements += 1,
            "Chaos" => chaos = true,
            _ => return None,
        }
    }
    Some((elements, chaos))
}

/// The trade category for an item and a short label for it ("Ring").
fn category(item: &Item) -> Option<(&'static str, &'static str)> {
    if game::current() == game::Game::Poe2 {
        return poe2_category(item);
    }
    if item.base.contains("Cluster Jewel") {
        return Some(("jewel.cluster", "Cluster Jewel"));
    }
    Some(match item.class.as_str() {
        "Body Armours" => ("armour.chest", "Body Armour"),
        "Boots" => ("armour.boots", "Boots"),
        "Gloves" => ("armour.gloves", "Gloves"),
        "Helmets" => ("armour.helmet", "Helmet"),
        "Shields" => ("armour.shield", "Shield"),
        "Quivers" => ("armour.quiver", "Quiver"),
        "Amulets" => ("accessory.amulet", "Amulet"),
        "Belts" => ("accessory.belt", "Belt"),
        "Rings" => ("accessory.ring", "Ring"),
        "Jewels" => ("jewel.base", "Jewel"),
        "Abyss Jewels" => ("jewel.abyss", "Abyss Jewel"),
        "Bows" => ("weapon.bow", "Bow"),
        "Claws" => ("weapon.claw", "Claw"),
        "Daggers" => ("weapon.basedagger", "Dagger"),
        "Rune Daggers" => ("weapon.runedagger", "Rune Dagger"),
        "One Hand Axes" => ("weapon.oneaxe", "One-Handed Axe"),
        "One Hand Swords" => ("weapon.basesword", "One-Handed Sword"),
        "Thrusting One Hand Swords" => ("weapon.rapier", "Rapier"),
        "One Hand Maces" => ("weapon.basemace", "One-Handed Mace"),
        "Sceptres" => ("weapon.sceptre", "Sceptre"),
        "Staves" => ("weapon.basestaff", "Staff"),
        "Warstaves" => ("weapon.warstaff", "Warstaff"),
        "Two Hand Axes" => ("weapon.twoaxe", "Two-Handed Axe"),
        "Two Hand Maces" => ("weapon.twomace", "Two-Handed Mace"),
        "Two Hand Swords" => ("weapon.twosword", "Two-Handed Sword"),
        "Wands" => ("weapon.wand", "Wand"),
        "Fishing Rods" => ("weapon.rod", "Fishing Rod"),
        "Maps" => ("map", "Map"),
        c if c.ends_with("Flasks") => ("flask", "Flask"),
        _ => return None,
    })
}

/// PoE2's item classes and trade categories (/api/trade2/data/filters).
fn poe2_category(item: &Item) -> Option<(&'static str, &'static str)> {
    Some(match item.class.as_str() {
        "Body Armours" => ("armour.chest", "Body Armour"),
        "Boots" => ("armour.boots", "Boots"),
        "Gloves" => ("armour.gloves", "Gloves"),
        "Helmets" => ("armour.helmet", "Helmet"),
        "Shields" => ("armour.shield", "Shield"),
        "Bucklers" => ("armour.buckler", "Buckler"),
        "Foci" => ("armour.focus", "Focus"),
        "Quivers" => ("armour.quiver", "Quiver"),
        "Amulets" => ("accessory.amulet", "Amulet"),
        "Belts" => ("accessory.belt", "Belt"),
        "Rings" => ("accessory.ring", "Ring"),
        "Jewels" => ("jewel", "Jewel"),
        "Bows" => ("weapon.bow", "Bow"),
        "Crossbows" => ("weapon.crossbow", "Crossbow"),
        "Claws" => ("weapon.claw", "Claw"),
        "Daggers" => ("weapon.dagger", "Dagger"),
        "One Hand Axes" => ("weapon.oneaxe", "One-Handed Axe"),
        "One Hand Swords" => ("weapon.onesword", "One-Handed Sword"),
        "One Hand Maces" => ("weapon.onemace", "One-Handed Mace"),
        "Spears" => ("weapon.spear", "Spear"),
        "Flails" => ("weapon.flail", "Flail"),
        "Two Hand Axes" => ("weapon.twoaxe", "Two-Handed Axe"),
        "Two Hand Maces" => ("weapon.twomace", "Two-Handed Mace"),
        "Two Hand Swords" => ("weapon.twosword", "Two-Handed Sword"),
        "Quarterstaves" => ("weapon.warstaff", "Quarterstaff"),
        "Talismans" => ("weapon.talisman", "Talisman"),
        "Sceptres" => ("weapon.sceptre", "Sceptre"),
        "Staves" => ("weapon.staff", "Staff"),
        "Wands" => ("weapon.wand", "Wand"),
        "Fishing Rods" => ("weapon.rod", "Fishing Rod"),
        "Life Flasks" => ("flask.life", "Life Flask"),
        "Mana Flasks" => ("flask.mana", "Mana Flask"),
        "Charms" => ("flask.charm", "Charm"),
        "Waystones" => ("map.waystone", "Waystone"),
        "Tablet" | "Tablets" | "Precursor Tablets" => ("map.tablet", "Tablet"),
        "Relics" => ("sanctum.relic", "Relic"),
        _ => return None,
    })
}

/// PoE2's spirit (sceptres) and rune sockets, as unticked filters.
fn poe2_filters_for(item: &Item) -> Vec<Filter> {
    let mut filters = Vec::new();
    if let Some(spirit) = item.spirit.filter(|v| *v > 0.0) {
        let target = Target::Property("equipment_filters", "spirit");
        filters.push(Filter::new(format!("Spirit {spirit}"), target, spirit, None, false, Section::Weapon));
    }
    if item.rune_sockets > 0 {
        let n = item.rune_sockets as f64;
        let target = Target::Property("equipment_filters", "rune_sockets");
        let mut f = Filter::new(format!("Rune sockets {n}"), target, n, None, false, Section::Base);
        // A socket count isn't a roll: the item's own, not a fraction of it.
        f.min = Some(n);
        f.slider = None;
        f.draw_slider = false;
        f.step = 1.0;
        f.max_roll = false;
        filters.push(f);
    }
    filters
}

/// Which of an item's mods apply to the item itself, which the trade site
/// lists as separate "(Local)" stats (or, for tinctures, as stats worded
/// like other items' ones).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Local {
    None,
    /// All of them: damage, attack speed, accuracy, leech, poison.
    Weapon,
    /// Only its defences. Accuracy or attack speed on gloves is global, and
    /// the "(Local)" stat would match no gloves at all.
    Armour,
    /// No "(Local)" stats, but `TINCTURE_STATS` instead of the flask's or
    /// jewel's stat with the same text.
    Tincture,
}

impl Local {
    fn of(item: &Item) -> Local {
        if item.class == "Tinctures" {
            return Local::Tincture;
        }
        match category(item) {
            Some((c, _)) if c.starts_with("weapon.") => Local::Weapon,
            // Like APT: a quiver's mods are all global.
            Some(("armour.quiver", _)) => Local::None,
            Some((c, _)) if c.starts_with("armour.") => Local::Armour,
            _ => Local::None,
        }
    }

    fn applies(self, template: &str) -> bool {
        match self {
            Local::None | Local::Tincture => false,
            Local::Weapon => true,
            Local::Armour => ["Armour", "Evasion", "Energy Shield"].iter().any(|d| template.contains(d)),
        }
    }
}

/// What `find_stat` found for a mod.
#[derive(Debug, PartialEq)]
struct Found {
    target: Target,
    /// The mod's value is searched times this: -1 for "reduced" as
    /// "increased".
    sign: f64,
    /// For each of the mod's numbers, whether it's part of the stat's text
    /// ("for 4 seconds") rather than its value. Empty when none is.
    fixed: Vec<bool>,
}

impl Found {
    /// The mod's value and roll range, without its fixed numbers.
    fn roll(&self, values: &[f64], ranges: &[Option<(f64, f64)>]) -> Option<(f64, Option<(f64, f64)>)> {
        if self.fixed.is_empty() {
            return mod_value(values, ranges);
        }
        let keep = |i: &usize| !self.fixed[*i];
        let values: Vec<f64> = (0..values.len()).filter(keep).map(|i| values[i]).collect();
        let ranges: Vec<_> = match ranges.len() == self.fixed.len() {
            true => (0..ranges.len()).filter(keep).map(|i| ranges[i]).collect(),
            false => Vec::new(),
        };
        mod_value(&values, &ranges)
    }
}

/// A mod's trade stat by its text, its numbers made `#` (`values` are
/// those numbers). Some stats have fixed numbers in their text ("#% chance
/// to gain Onslaught for 4 seconds on Kill"): a mod matches one of those
/// when its own numbers there are the same.
fn find_stat(data: &TradeData, kind: ModKind, template: &str, values: &[f64], local: Local) -> Option<Found> {
    let group = kind.trade_group().to_string();
    let mut wordings = vec![(template.to_string(), 1.0)];
    // "10% reduced X" is searched as "#% increased X" with a negative value.
    for (from, to) in [("reduced", "increased"), ("less", "more")] {
        if template.contains(from) {
            wordings.push((template.replace(from, to), -1.0));
        }
    }
    if let Some(rest) = template.strip_prefix("-#") {
        wordings.push((format!("+#{rest}"), -1.0));
    }
    // A local mod tries every wording's "(Local)" stat first, so a weapon's
    // "reduced Attack Speed" finds the local stat, not the global one. Any
    // other mod tries them last: a stat only listed as "(Local)" ("#%
    // increased Energy Shield") still matches, as in APT.
    let with_locals = |wordings: Vec<(String, f64)>| {
        let locals = wordings.iter().map(|(t, sign)| (format!("{t} (Local)"), *sign)).collect::<Vec<_>>();
        if local.applies(template) {
            locals.into_iter().chain(wordings).collect::<Vec<_>>()
        } else {
            wordings.into_iter().chain(locals).collect()
        }
    };
    // PoE2's stats have no "+": "# to Accuracy Rating", "#% to Cold
    // Resistance" (the item's text still has it). Tried only when nothing
    // above matches, so PoE1's matches stay as they were.
    let unsigned: Vec<(String, f64)> =
        wordings.iter().filter_map(|(t, sign)| t.strip_prefix('+').map(|r| (r.to_string(), *sign))).collect();
    let candidates = with_locals(wordings);
    let unsigned = with_locals(unsigned);
    let found = |text: &str, sign, fixed| {
        let mut ids = data.stat_ids(&group, text);
        // PoE2's desecrated mods are explicit mods once revealed: an item
        // like this one may have the mod either way.
        if kind == ModKind::Desecrated
            && let Some(explicit) = data.stat_ids("explicit", text)
        {
            ids.get_or_insert_with(Vec::new).extend(explicit);
        }
        let ids = ids.or_else(|| data.stat_ids("pseudo", text))?;
        Some(Found { target: stat_target(select_stats(ids, local)), sign, fixed })
    };
    // Some stats are only searchable as pseudo stats (item rarity, for one).
    let search = |candidates: &[(String, f64)]| {
        candidates.iter().find_map(|(t, sign)| found(t, *sign, Vec::new())).or_else(|| {
            candidates.iter().find_map(|(t, sign)| {
                let (text, slots) = data.numbered.get(&(group.clone(), t.clone()))?.iter().find(|(_, slots)| {
                    slots.len() == values.len()
                        && slots.iter().zip(values).all(|(slot, v)| slot.is_none_or(|n| (n - v).abs() < 1e-9))
                })?;
                found(text, *sign, slots.iter().map(Option::is_some).collect())
            })
        })
    };
    search(&candidates).or_else(|| search(&unsigned))
}

/// For each `#` in a stat's numbered-out text, the fixed number it was, or
/// `None` for the stat's own value.
type Slots = Vec<Option<f64>>;

/// A stat's text with every number made `#`, and for each `#` the fixed
/// number it was, or `None` for the stat's own value: "#% chance to gain
/// Onslaught for 4 seconds on Kill" -> ("#% chance to gain Onslaught for #
/// seconds on Kill", [None, Some(4)]).
fn number_slots(text: &str) -> (String, Slots) {
    let mut template = String::new();
    let mut slots = Vec::new();
    for (i, part) in text.split('#').enumerate() {
        if i > 0 {
            template.push('#');
            slots.push(None);
        }
        let (t, values) = templatize(part);
        template.push_str(&t);
        slots.extend(values.into_iter().map(Some));
    }
    (template, slots)
}

/// Of stats with the same text, the ones an item's mod can be: a
/// tincture's are `TINCTURE_STATS`, and nothing else's are.
fn select_stats(ids: Vec<String>, local: Local) -> Vec<String> {
    let tincture = |id: &String| id.rsplit_once('.').is_some_and(|(_, stat)| TINCTURE_STATS.contains(&stat));
    let (tinctures, others): (Vec<_>, Vec<_>) = ids.into_iter().partition(tincture);
    if others.is_empty() || (local == Local::Tincture && !tinctures.is_empty()) { tinctures } else { others }
}

/// One stat, or any of several with the same text.
fn stat_target(mut ids: Vec<String>) -> Target {
    match ids.len() {
        1 => Target::Stat(ids.remove(0)),
        _ => Target::AnyStat(ids),
    }
}

/// "+# to Strength and Dexterity" -> Some([1, 1, 0]) (str, dex, int).
fn attribute_parts(template: &str) -> Option<[f64; 3]> {
    let t = template.strip_prefix("+# to ")?;
    if t == "all Attributes" {
        return Some([1.0; 3]);
    }
    let mut parts = [0.0; 3];
    for word in t.split(" and ") {
        match word {
            "Strength" => parts[0] = 1.0,
            "Dexterity" => parts[1] = 1.0,
            "Intelligence" => parts[2] = 1.0,
            _ => return None,
        }
    }
    Some(parts)
}

/// Filters in the spirit of Awakened PoE Trade's default ("pseudo") preset:
/// life, mana, attributes and resistances from any mod (implicit and crafted
/// included) become totals, and only total life and the resistance totals
/// start ticked. Other mods are listed, unticked, crafted ones too (like
/// APT; they're searched as crafted).
/// A mod's searchable value and its roll range: the number itself, or the
/// average for "Adds # to # ...", which the trade site searches by.
fn mod_value(values: &[f64], ranges: &[Option<(f64, f64)>]) -> Option<(f64, Option<(f64, f64)>)> {
    let range = |i: usize| if ranges.len() == values.len() { ranges[i] } else { None };
    match values {
        [] => None,
        [a, b] => {
            let r = match (range(0), range(1)) {
                (Some((alo, ahi)), Some((blo, bhi))) => Some(((alo + blo) / 2.0, (ahi + bhi) / 2.0)),
                _ => None,
            };
            Some(((a + b) / 2.0, r))
        }
        [v, ..] => Some((*v, range(0))),
    }
}

fn round_down(v: f64, step: f64) -> f64 {
    ((v / step) + 1e-9).floor() * step
}

/// Minimum, slider range, step and max-roll flag for a filter's value.
/// `fine`: hundredths even for a whole-number value (crit chance: 7.00%).
fn defaults(value: f64, bounds: Option<(f64, f64)>, fine: bool) -> (Option<f64>, Option<(f64, f64)>, f64, bool) {
    if value <= 0.0 {
        // "reduced"/negative stats are searched without a minimum.
        return (None, None, 1.0, false);
    }
    let fractional = |v: f64| (v - v.round()).abs() > 1e-6;
    let step = if fine || fractional(value) || bounds.is_some_and(|(lo, hi)| fractional(lo) || fractional(hi)) {
        0.01
    } else {
        1.0
    };
    let tidy = |v: f64| (v / step).round() / (1.0 / step);
    match bounds {
        Some((lo, hi)) if hi > lo => {
            if value >= hi - 1e-9 {
                (Some(tidy(value)), Some((lo.min(value), hi)), step, true)
            } else {
                // Roll - 10%, raised to the tier's floor like APT: a roll at
                // the bottom of its tier is searched at the floor. A roll
                // below its tier (corrupted, or a total of odd parts) still
                // finds the item itself, and the slider reaches down to it.
                let floor = lo.min(value);
                let min = tidy(round_down(value * min_fraction(), step)).max(floor);
                (Some(min), Some((floor, hi)), step, false)
            }
        }
        // A fixed roll: nothing to slide.
        Some(_) => (Some(tidy(value)), None, step, false),
        None => {
            let lo = round_down(value * 0.5, step);
            let hi = ((value * 1.25) / step).ceil() * step;
            (Some(tidy(round_down(value * min_fraction(), step))), Some((lo, hi)), step, false)
        }
    }
}

/// Filters in the spirit of Awakened PoE Trade's default ("pseudo") preset:
/// life, mana, attributes and resistances from any mod (implicit and crafted
/// included) become totals, and only total life and the resistance totals
/// start ticked. Other mods are listed, unticked, crafted ones too (like
/// APT; they're searched as crafted).
/// Weapons also get DPS filters, from their properties.
/// Weapon DPS, from the properties the item shows. `ticked`: the main
/// damage type starts ticked.
fn weapon_filters(item: &Item, ticked: bool) -> Vec<Filter> {
    let mut filters = Vec::new();
    if category(item).is_some_and(|(c, _)| c.starts_with("weapon."))
        && let Some(aps) = item.attacks_per_second
    {
        let pdps = (item.physical_damage.unwrap_or(0.0) * aps).round();
        let edps = (item.elemental_damage * aps).round();
        if pdps > 0.0 {
            let label = format!("Physical DPS {pdps}");
            filters.push(Filter::new(
                label,
                Target::Property("weapon_filters", "pdps"),
                pdps,
                None,
                ticked && pdps >= edps,
                Section::Weapon,
            ));
        }
        if edps > 0.0 {
            let label = format!("Elemental DPS {edps}");
            filters.push(Filter::new(
                label,
                Target::Property("weapon_filters", "edps"),
                edps,
                None,
                ticked && edps > pdps,
                Section::Weapon,
            ));
        }
        let label = format!("Attacks per second {aps:.2}");
        filters.push(Filter::new(label, Target::Property("weapon_filters", "aps"), aps, None, false, Section::Weapon));
        if let Some(crit) = item.critical_chance {
            let label = match game::current() {
                game::Game::Poe1 => format!("Critical strike chance {crit:.2}%"),
                game::Game::Poe2 => format!("Critical hit chance {crit:.2}%"),
            };
            let (min, slider, step, max_roll) = defaults(crit, None, true);
            filters.push(Filter {
                label,
                target: Target::Property("weapon_filters", "crit"),
                min,
                slider,
                draw_slider: true,
                step,
                max_roll,
                default_on: false,
                section: Section::Weapon,
                tier: None,
                increased: None,
                max: None,
                roll: Some(crit),
            });
        }
    }
    filters
}

/// Local "#% increased Armour"-style mods, summed for armour, evasion,
/// energy shield and ward.
fn local_defence_increase(item: &Item) -> [f64; 4] {
    let mut inc = [0.0; 4];
    for m in &item.mods {
        let (template, values) = templatize(&m.text);
        let Some(rest) = template.strip_prefix("#% increased ") else { continue };
        let Some(&value) = values.first() else { continue };
        let parts: &[usize] = match rest {
            "Armour" => &[0],
            "Evasion Rating" => &[1],
            "Energy Shield" => &[2],
            "Ward" => &[3],
            "Armour and Evasion" => &[0, 1],
            "Armour and Energy Shield" => &[0, 2],
            "Evasion and Energy Shield" => &[1, 2],
            "Armour, Evasion and Energy Shield" => &[0, 1, 2],
            _ => continue,
        };
        for &i in parts {
            inc[i] += value;
        }
    }
    inc
}

/// Armour pieces' defences, unticked. The trade site searches defences as
/// at 20% quality (like APT), so lower quality is scaled up.
fn armour_filters(item: &Item) -> Vec<Filter> {
    let mut filters = Vec::new();
    if !category(item).is_some_and(|(c, _)| c.starts_with("armour.")) {
        return filters;
    }
    let inc = local_defence_increase(item);
    let quality = item.quality as f64;
    let defences = [
        ("Armour", item.armour, "ar"),
        ("Evasion", item.evasion, "ev"),
        ("Energy shield", item.energy_shield, "es"),
        ("Ward", item.ward, "ward"),
    ];
    for (i, (name, value, key)) in defences.into_iter().enumerate() {
        let Some(value) = value.filter(|v| *v > 0.0) else { continue };
        let at20 = (value * (100.0 + inc[i] + quality.max(20.0)) / (100.0 + inc[i] + quality)).round();
        let label = if at20 > value { format!("{name} {at20} (at 20% quality)") } else { format!("{name} {at20}") };
        filters.push(Filter::new(label, Target::Property("armour_filters", key), at20, None, false, Section::Armour));
    }
    if let Some(block) = item.block.filter(|v| *v > 0.0) {
        let label = format!("Chance to block {block}%");
        filters.push(Filter::new(
            label,
            Target::Property("armour_filters", "block"),
            block,
            None,
            false,
            Section::Armour,
        ));
    }
    filters
}

/// Current map bases name their tier, e.g. "Map (Tier 16)" or "Blighted Map
/// (Tier 5)"; older ones ("Strand Map") don't.
/// A PoE2 waystone's properties (its tier is above): what drops more
/// (ticked), and what makes it harder (unticked). Labels as the item says
/// them; PoE2's trade site has a filter for each.
fn waystone_filters(item: &Item) -> Vec<Filter> {
    let props: [(&str, &str, &'static str, bool, bool); 8] = [
        ("Waystone Drop Chance", "Waystone drop chance", "map_bonus", true, true),
        ("Item Rarity", "Rarity", "map_iir", true, true),
        ("Gold Found", "Gold", "map_gold", true, false),
        ("Pack Size", "Pack size", "map_packsize", true, false),
        ("Monster Pack Size", "Pack size", "map_packsize", true, false),
        ("Magic Monsters", "Magic monsters", "map_magic_monsters", true, false),
        ("Rare Monsters", "Rare monsters", "map_rare_monsters", true, false),
        ("Revives Available", "Revives", "map_revives", false, false),
    ];
    let mut filters = Vec::new();
    for (prop, name, key, percent, on) in props {
        let value = item
            .map_props
            .iter()
            .find(|(label, _)| label == prop)
            .and_then(|(_, v)| v.trim_start_matches('+').trim_end_matches('%').parse::<f64>().ok());
        let Some(value) = value.filter(|v| *v > 0.0) else { continue };
        if filters.iter().any(|f: &Filter| f.target == Target::Property("map_filters", key)) {
            continue;
        }
        let label = if percent { format!("{name} +{value}%") } else { format!("{name} {value}") };
        filters.push(Filter::new(label, Target::Property("map_filters", key), value, None, on, Section::Map));
    }
    filters
}

fn tiered_map_base(item: &Item) -> bool {
    item.class == "Maps" && item.base.contains("(Tier ")
}

/// A map is priced by what it gives, not by its mods (those are for the map
/// check): its tier (unless the base names it), its drop bonuses (ticked),
/// pack size and rarity (unticked), and blight.
fn map_filters(item: &Item) -> Vec<Filter> {
    let mut filters = Vec::new();
    if let Some(tier) = item.map_tier.filter(|_| !tiered_map_base(item)) {
        let t = tier as f64;
        let label = format!("Tier {tier}");
        filters.push(Filter::new(
            label,
            Target::Property("map_filters", "map_tier"),
            t,
            Some((t, t)),
            true,
            Section::Map,
        ));
    }
    if item.class == "Waystones" {
        filters.extend(waystone_filters(item));
        return filters;
    }
    // Since 3.25 a map's worth is mostly its "More ..." drop bonuses; old
    // maps still have item quantity.
    let props = [
        ("More Currency", "More currency", Target::Stat("pseudo.pseudo_map_more_currency_drops".into()), true),
        ("More Scarabs", "More scarabs", Target::Stat("pseudo.pseudo_map_more_scarab_drops".into()), true),
        ("More Maps", "More maps", Target::Stat("pseudo.pseudo_map_more_map_drops".into()), true),
        ("More Divination Cards", "More cards", Target::Stat("pseudo.pseudo_map_more_card_drops".into()), true),
        ("Item Quantity", "Quantity", Target::Property("map_filters", "map_iiq"), true),
        ("Monster Pack Size", "Pack size", Target::Property("map_filters", "map_packsize"), false),
        ("Item Rarity", "Rarity", Target::Property("map_filters", "map_iir"), false),
    ];
    for (prop, name, target, on) in props {
        let value = item
            .map_props
            .iter()
            .find(|(label, _)| label == prop)
            .and_then(|(_, v)| v.trim_start_matches('+').trim_end_matches('%').parse::<f64>().ok());
        let Some(value) = value.filter(|v| *v > 0.0) else { continue };
        let label = format!("{name} +{value}%");
        filters.push(Filter::new(label, target, value, None, on, Section::Map));
    }
    let flag = |label: &str, key: &'static str| Filter {
        label: label.to_string(),
        target: Target::Flag("map_filters", key),
        min: None,
        slider: None,
        draw_slider: true,
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: Section::Map,
        tier: None,
        increased: None,
        max: None,
        roll: None,
    };
    if item.base.starts_with("Blight-ravaged") {
        filters.push(flag("Blight-ravaged", "map_uberblighted"));
    } else if item.base.starts_with("Blighted") {
        filters.push(flag("Blighted", "map_blighted"));
    }
    filters
}

/// A Facetor's Lens is worth its experience: listings with at least as
/// much. The slider (and wheel, 5% a step) widens it to smaller lenses.
fn stored_experience_filter(xp: u64) -> Filter {
    let xp = xp as f64;
    Filter {
        label: format!("Stored experience {}", group_thousands(xp as u64)),
        target: Target::Property("misc_filters", "stored_experience"),
        min: Some(xp),
        slider: Some((0.0, xp)),
        draw_slider: true,
        step: (xp / 20.0).round().max(1.0),
        max_roll: false,
        default_on: true,
        section: Section::Base,
        tier: None,
        increased: None,
        max: None,
        roll: Some(xp),
    }
}

/// 750000000 -> "750,000,000", as the game writes it.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Like APT: the item's memory strands as a minimum, ticked at 60+.
fn memory_strands_filter(item: &Item) -> Option<Filter> {
    let strands = item.memory_strands? as f64;
    Some(Filter {
        label: format!("Memory Strands {strands}"),
        target: Target::Property("misc_filters", "memory_level"),
        min: Some(strands),
        slider: Some((0.0, 100.0)),
        draw_slider: true,
        step: 1.0,
        max_roll: false,
        default_on: strands >= 60.0,
        section: Section::Base,
        tier: None,
        increased: None,
        max: None,
        roll: Some(strands),
    })
}

/// Like APT for uniques: no totals, only the mods that roll (fixed ones
/// don't tell copies apart), all unticked.
fn unique_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let local = Local::of(item);
    let mut filters = weapon_filters(item, false);
    filters.extend(memory_strands_filter(item));
    let (options, picked) = option_filters(item, data);
    filters.extend(options);
    for (i, m) in item.mods.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        let (template, values) = templatize(&m.text);
        let Some(found) = find_stat(data, m.kind, &template, &values, local) else { continue };
        let Some((value, Some((lo, hi)))) = found.roll(&values, &m.ranges) else { continue };
        let section = mod_section(m, Section::Mod);
        let mut filter = if hi > lo {
            Filter::unique(m.text.clone(), found.target, value * found.sign, (lo, hi), section)
        } else if m.increased.is_some() {
            // A fixed mod a catalyst raised: copies differ by their quality,
            // so it's listed, searched at its value.
            Filter {
                draw_slider: false,
                ..Filter::new(m.text.clone(), found.target, value * found.sign, Some((lo, hi)), false, section)
            }
        } else {
            continue;
        };
        filter.tier = m.tier;
        filter.increased = m.increased;
        filters.push(filter);
    }
    filters
}

const CLUSTER_GRANT: &str = "Added Small Passive Skills grant: ";
/// A cluster jewel's passive count: searched exactly, since fewer passives
/// is better on a large cluster and more on a small one.
const CLUSTER_PASSIVES: &str = "enchant.stat_3086156145";

/// Mods that are one of a list rather than a number: cluster jewel grants,
/// anointed notables, "Allocates X if you have the matching modifier on
/// Forbidden Flesh". They're what the item is, so they start ticked. Also
/// returns which mods (by index) they used.
fn option_filters(item: &Item, data: &TradeData) -> (Vec<Filter>, Vec<usize>) {
    let mut filters = Vec::new();
    let mut picked = Vec::new();
    let option_stat = |group: &str, text: &str| -> Option<Target> {
        let id = data.stats.get(&(group.to_string(), text.to_string()))?;
        let (id, option) = id.split_once('|')?;
        Some(Target::StatOption(id.to_string(), option.parse().ok()?))
    };
    let filter = |label: &str, target: Target, kind: ModKind| Filter {
        label: label.to_string(),
        target,
        min: None,
        slider: None,
        draw_slider: true,
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: kind_section(kind, Section::Mod),
        tier: None,
        increased: None,
        max: None,
        roll: None,
    };
    // Some grants are two lines ("… Damage with Bows" and "… Damage Over
    // Time with Bow Skills"): the stat list has them joined.
    let grants: Vec<usize> = (0..item.mods.len()).filter(|&i| item.mods[i].text.starts_with(CLUSTER_GRANT)).collect();
    if grants.len() > 1 {
        let joined = grants.iter().map(|&i| item.mods[i].text.as_str()).collect::<Vec<_>>().join("\n");
        if let Some(target) = option_stat("enchant", &joined) {
            let label =
                grants.iter().map(|&i| &item.mods[i].text[CLUSTER_GRANT.len()..]).collect::<Vec<_>>().join(", ");
            filters.push(filter(&format!("{CLUSTER_GRANT}{label}"), target, ModKind::Enchant));
            picked.extend(&grants);
        }
    }
    for (i, m) in item.mods.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        if let Some(target) = option_stat(m.kind.trade_group(), &m.text) {
            filters.push(filter(&m.text, target, m.kind));
            picked.push(i);
        }
    }
    (filters, picked)
}

/// An eldritch implicit's tier as a pseudo stat ("Searing Exarch Implicit
/// Modifier (#)", at least this item's), or just the flag without advanced
/// copy's tier names.
fn eldritch_filters(item: &Item, data: &TradeData, on: bool) -> Vec<Filter> {
    let mut filters = Vec::new();
    let tiers = ["Lesser", "Greater", "Grand", "Exceptional", "Exquisite", "Perfect"];
    for (seen, name, pseudo, flag, section) in [
        (item.searing, "Searing Exarch", "Searing Exarch Implicit Modifier (#)", "searing_item", Section::Exarch),
        (item.tangled, "Eater of Worlds", "Eater of Worlds Implicit Modifier (#)", "tangled_item", Section::Eater),
    ] {
        let Some(tier) = seen else { continue };
        let stat = data.stats.get(&("pseudo".to_string(), pseudo.to_string()));
        let (label, target, min, slider) = match (tier, stat) {
            (Some(t), Some(id)) => (
                format!("{name} implicit: {}+", tiers[(t as usize - 1).min(5)]),
                Target::Stat(id.clone()),
                Some(t as f64),
                Some((1.0, 6.0)),
            ),
            _ => (format!("{name} item"), Target::Flag("misc_filters", flag), None, None),
        };
        filters.push(Filter {
            label,
            target,
            min,
            slider,
            draw_slider: true,
            step: 1.0,
            max_roll: false,
            default_on: on,
            section,
            tier: None,
            increased: None,
            max: None,
            roll: tier.map(|t| t as f64),
        });
    }
    filters
}

/// An eldritch implicit's own row already implies its altar, so the altar's
/// tier/flag filter goes when one is listed. It stays when there's nothing
/// else to search the altar by: plain copy (no headers), or implicits only
/// counted in totals (resistances, life).
fn drop_covered_eldritch(filters: &mut Vec<Filter>) {
    let altar_only = |f: &Filter| match &f.target {
        Target::Flag(..) => true,
        Target::Stat(id) => id.starts_with("pseudo."),
        _ => false,
    };
    let covered: Vec<Section> = filters
        .iter()
        .filter(|f| matches!(f.section, Section::Exarch | Section::Eater) && !altar_only(f))
        .map(|f| f.section)
        .collect();
    filters.retain(|f| !(covered.contains(&f.section) && altar_only(f)));
}

/// A cluster jewel's passive count and jewel sockets, searched as they are.
fn exact_cluster_counts(filters: &mut [Filter]) {
    for f in filters {
        if let Target::Stat(id) = &f.target
            && (id == CLUSTER_PASSIVES || id == "enchant.stat_4079888060")
            && let Some(roll) = f.roll
        {
            f.min = Some(roll);
            f.max = (id == CLUSTER_PASSIVES).then_some(roll);
            f.default_on = true;
            f.max_roll = false;
        }
    }
}

/// The panel groups implicits and enchants under their own headings and
/// colors fractured and crafted mods, so labels are the mod's text alone.
fn kind_section(kind: ModKind, section: Section) -> Section {
    match (kind, section) {
        (ModKind::Implicit, Section::Mod) => Section::Implicit,
        (ModKind::Enchant, Section::Mod) => Section::Enchant,
        (ModKind::Crafted, Section::Mod) => Section::Crafted,
        // PoE2's rune mods: like enchants, they come from outside the item's
        // own mods.
        (ModKind::Rune, Section::Mod) => Section::Enchant,
        _ => section,
    }
}

/// A mod's section by its kind, an eldritch implicit's by its altar, a
/// corruption implicit's as its own.
fn mod_section(m: &ModLine, section: Section) -> Section {
    match (kind_section(m.kind, section), m.eldritch) {
        (Section::Implicit, Some(Eldritch::Exarch)) => Section::Exarch,
        (Section::Implicit, Some(Eldritch::Eater)) => Section::Eater,
        (Section::Implicit, None) if m.corruption => Section::Corruption,
        (section, _) => section,
    }
}

/// A mod as a stat filter of its own kind (explicit, fractured, ...).
fn mod_filter(m: &ModLine, data: &TradeData, local: Local, on: bool, section: Section) -> Option<Filter> {
    let (template, values) = templatize(&m.text);
    let found = find_stat(data, m.kind, &template, &values, local)?;
    let roll = found.roll(&values, &m.ranges);
    let Found { target, sign, .. } = found;
    let section = mod_section(m, section);
    let mut filter = match roll {
        Some((value, range)) => Filter {
            draw_slider: range.is_some(),
            ..Filter::new(m.text.clone(), target, value * sign, range, on, section)
        },
        // No number ("Warcries cannot Exert Travel Skills"), or only fixed
        // ones: searched by whether the item has it, with nothing to slide.
        None => Filter {
            label: m.text.replace('\n', " "),
            target,
            min: None,
            slider: None,
            draw_slider: true,
            step: 1.0,
            max_roll: false,
            default_on: on,
            section,
            tier: None,
            increased: None,
            max: None,
            roll: None,
        },
    };
    filter.tier = m.tier;
    filter.increased = m.increased;
    Some(filter)
}

/// A limb is its implicits: each one ticked. Advanced copy heads them
/// "{ Implicit Modifier }" without an "(implicit)" marker, so they're read
/// as implicits whatever the parser made of them. With a roll range, the
/// maximum is the tier's best roll: "Lesser", plain and "Greater" limbs
/// share a trade base, and a Lesser arm's +15 Strength shouldn't be
/// compared with a plain arm's +29.
fn viscera_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    item.mods
        .iter()
        .filter_map(|m| {
            let m = ModLine { kind: ModKind::Implicit, ..m.clone() };
            let mut filter = mod_filter(&m, data, Local::None, true, Section::Mod)?;
            let (_, values) = templatize(&m.text);
            if let Some((_, Some((lo, hi)))) = mod_value(&values, &m.ranges)
                && hi > lo
                && filter.roll.is_some_and(|r| r > 0.0)
            {
                filter.max = Some(hi);
            }
            Some(filter)
        })
        .collect()
}

/// APT's "base item" preset: what makes the item worth as a crafting base.
/// Like APT: the gem's level (ticked at its maximum, which is where gems
/// sell) and quality (ticked from 16% on a plain gem, 20% on a transfigured
/// one, whose quality is harder to come by). Corruption is an option.
///
/// Only minimums, like APT: a better gem listed for less is competition
/// (and past the cap takes a corruption, so an uncorrupted gem's search
/// never finds one). Listings show each gem's level and quality.
fn gem_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let mut filters = Vec::new();
    let property = |label: String, key: &'static str, value: u32, hi: f64, on: bool| Filter {
        label,
        target: Target::Property("misc_filters", key),
        min: Some(value as f64),
        slider: Some((0.0f64.max(value as f64 - 10.0).min(1.0), hi.max(value as f64))),
        draw_slider: true,
        step: 1.0,
        max_roll: false,
        default_on: on,
        section: Section::Gem,
        tier: None,
        increased: None,
        max: None,
        roll: Some(value as f64),
    };
    if let Some(level) = item.gem_level {
        let on = item.gem_max_level || level >= 20;
        filters.push(property(format!("Gem level {level}"), "gem_level", level, 21.0, on));
    }
    if item.quality > 0 {
        let transfigured = data.transfigured.contains_key(&item.base);
        let on = item.quality >= 20 || (item.quality >= 16 && !transfigured);
        filters.push(property(format!("Quality {}%", item.quality), "quality", item.quality, 23.0, on));
    }
    filters
}

fn base_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let local = Local::of(item);
    let mut filters = Vec::new();
    let flag = |label: String, key: &'static str| Filter {
        label,
        target: Target::Flag("misc_filters", key),
        min: None,
        slider: None,
        draw_slider: true,
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: Section::Base,
        tier: None,
        increased: None,
        max: None,
        roll: None,
    };
    if let Some(ilvl) = item.item_level {
        // Higher item levels don't add mod tiers (APT caps at 86). For a
        // linked base without crafting value, it hardly matters.
        let min = ilvl.min(86) as f64;
        filters.push(Filter {
            label: format!("Item level {ilvl}"),
            target: Target::Property("misc_filters", "ilvl"),
            min: Some(min),
            slider: Some((min.min(60.0), 86.0)),
            draw_slider: true,
            step: 1.0,
            max_roll: false,
            default_on: crafting_value(item),
            section: Section::Base,
            tier: None,
            increased: None,
            max: None,
            roll: Some(ilvl as f64),
        });
    }
    if item.fractured_item {
        filters.push(flag("Fractured item".into(), "fractured_item"));
    }
    if item.synthesised {
        filters.push(flag("Synthesised item".into(), "synthesised_item"));
    }
    filters.extend(influence_filters(item, true));
    filters.extend(eldritch_filters(item, data, true));
    filters.extend(memory_strands_filter(item));
    let (options, picked) = option_filters(item, data);
    filters.extend(options);
    // Influenced and fractured bases' implicits vary too much to match; a
    // linked base without crafting value is worth its links, not its rolls.
    let implicits = item.influences.is_empty() && !item.fractured_item;
    let valuable = crafting_value(item);
    for (i, m) in item.mods.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        let (on, section) = match m.kind {
            ModKind::Fractured => (true, Section::Fractured),
            ModKind::Enchant => (true, Section::Mod),
            ModKind::Implicit if implicits => (valuable, Section::Mod),
            // A magic base's top-tier mods are part of its value.
            ModKind::Explicit if item.rarity == "Magic" => (m.tier.is_some_and(|t| t <= 2), Section::Mod),
            _ => continue,
        };
        filters.extend(mod_filter(m, data, local, on, section));
    }
    filters.extend(armour_filters(item));
    filters.extend(poe2_filters_for(item));
    drop_covered_eldritch(&mut filters);
    exact_cluster_counts(&mut filters);
    filters
}

/// "Has Shaper influence" and the like, one per influence (an item can
/// have two).
fn influence_filters(item: &Item, on: bool) -> Vec<Filter> {
    item.influences
        .iter()
        .map(|&influence| {
            let key = match influence {
                "shaper" => "shaper_item",
                "elder" => "elder_item",
                "crusader" => "crusader_item",
                "redeemer" => "redeemer_item",
                "hunter" => "hunter_item",
                _ => "warlord_item",
            };
            let mut name = influence.to_string();
            name[..1].make_ascii_uppercase();
            Filter {
                label: format!("Has {name} influence"),
                target: Target::Flag("misc_filters", key),
                min: None,
                slider: None,
                draw_slider: true,
                step: 1.0,
                max_roll: false,
                default_on: on,
                section: Section::Influence(influence),
                tier: None,
                increased: None,
                max: None,
                roll: None,
            }
        })
        .collect()
}

/// A rare's mod as an unticked row of its own. A crafted mod is listed only
/// when the trade site has it as a crafted stat: one only searchable as a
/// pseudo stat (item rarity) would repeat the explicit mod's row.
fn listed_filter(m: &ModLine, data: &TradeData, local: Local) -> Option<Filter> {
    let filter = mod_filter(m, data, local, false, Section::Mod)?;
    let crafted_stat = match &filter.target {
        Target::Stat(id) => id.starts_with("crafted."),
        Target::AnyStat(ids) => ids.iter().all(|id| id.starts_with("crafted.")),
        _ => false,
    };
    (m.kind != ModKind::Crafted || crafted_stat).then_some(filter)
}

fn build_filters(item: &Item, data: &TradeData, preset: Preset) -> Vec<Filter> {
    let item = &join_wrapped(item, data);
    if (item.class == "Maps" || item.class == "Waystones") && item.rarity != "Unique" {
        return map_filters(item);
    }
    if item.rarity == "Unique" {
        return unique_filters(item, data);
    }
    if item.class == "Viscera" {
        return viscera_filters(item, data);
    }
    if item.rarity == "Gem" {
        return gem_filters(item, data);
    }
    if let Some(xp) = item.stored_experience {
        return vec![stored_experience_filter(xp)];
    }
    if item.warrant.is_some() {
        return warrant_filters(item, data);
    }
    if preset == Preset::Base {
        return base_filters(item, data);
    }
    let local = Local::of(item);
    let mut listed = Vec::new();
    // Fractured mods also count in totals, but can be searched as fractured.
    let mut fractured = Vec::new();
    let (mut elemental, mut chaos, mut life, mut mana) =
        (Amount::default(), Amount::default(), Amount::default(), Amount::default());
    let mut attributes = [Amount::default(); 3];
    let (options, picked) = option_filters(item, data);

    for (i, m) in item.mods.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        if m.kind == ModKind::Fractured {
            fractured.extend(mod_filter(m, data, local, false, Section::Fractured));
        }
        let (template, values) = templatize(&m.text);
        // A mod without a number can't add to a total.
        let Some((value, range)) = mod_value(&values, &m.ranges) else {
            if m.kind != ModKind::Fractured {
                listed.extend(listed_filter(m, data, local));
            }
            continue;
        };
        if let Some((elements, has_chaos)) = resist_parts(&template) {
            if elements > 0 {
                elemental.add(value, range, elements as f64);
            }
            if has_chaos {
                chaos.add(value, range, 1.0);
            }
            continue;
        }
        if let Some(parts) = attribute_parts(&template) {
            for (total, part) in attributes.iter_mut().zip(parts) {
                if part > 0.0 {
                    total.add(value, range, part);
                }
            }
            continue;
        }
        match template.as_str() {
            "+# to maximum Life" => {
                life.add(value, range, 1.0);
                continue;
            }
            "+# to maximum Mana" => {
                mana.add(value, range, 1.0);
                continue;
            }
            _ => {}
        }
        if m.kind == ModKind::Fractured {
            continue;
        }
        listed.extend(listed_filter(m, data, local));
    }

    let mut filters = Vec::new();

    filters.extend(weapon_filters(item, true));
    filters.extend(armour_filters(item));
    filters.extend(poe2_filters_for(item));
    filters.extend(influence_filters(item, false));
    filters.extend(eldritch_filters(item, data, false));
    filters.extend(memory_strands_filter(item));
    filters.extend(options);

    // Strength gives 1 life per 2, intelligence 1 mana per 2.
    let [strength, dexterity, intelligence] = attributes;
    let mut add = |text: &str, total: Amount, on: bool| {
        if !total.seen || total.value <= 0.0 {
            return;
        }
        let Some(id) = data.stats.get(&("pseudo".to_string(), text.to_string())) else { return };
        let value = total.value.floor();
        let label = text.replacen('#', &value.to_string(), 1);
        let bounds = total.bounds();
        filters.push(Filter {
            draw_slider: bounds.is_some(),
            ..Filter::new(label, Target::Stat(id.clone()), value, bounds, on, Section::Total)
        });
    };
    if life.seen {
        let mut total = life;
        if strength.seen {
            total.add(strength.value, strength.bounds(), 0.5);
        }
        add("+# total maximum Life", total, true);
    }
    add("+#% total Elemental Resistance", elemental, true);
    add("+#% total to Chaos Resistance", chaos, true);
    if mana.seen {
        let mut total = mana;
        if intelligence.seen {
            total.add(intelligence.value, intelligence.bounds(), 0.5);
        }
        add("+# total maximum Mana", total, false);
    }
    add("+# total to Strength", strength, false);
    add("+# total to Dexterity", dexterity, false);
    add("+# total to Intelligence", intelligence, false);

    filters.extend(no_life_filter(item, data));
    filters.extend(fractured);
    filters.extend(listed);
    drop_covered_eldritch(&mut filters);
    exact_cluster_counts(&mut filters);
    filters
}

/// The item with each mod the game wrapped onto more than one line made one
/// mod again, its lines joined by "\n" as the trade site's stat has them
/// ("#% increased Elemental Damage with Hits and Ailments for\neach type of
/// Elemental Ailment on Enemy"). A line is joined with the next of its group
/// only when it matches no stat by itself and the two together do.
fn join_wrapped(item: &Item, data: &TradeData) -> Item {
    let local = Local::of(item);
    let matches = |m: &ModLine| {
        let (template, values) = templatize(&m.text);
        find_stat(data, m.kind, &template, &values, local).is_some()
    };
    let mut mods: Vec<ModLine> = Vec::with_capacity(item.mods.len());
    for m in &item.mods {
        if let Some(last) = mods.last_mut()
            && last.group == m.group
            && last.kind == m.kind
            && !matches(last)
        {
            let mut joined = last.clone();
            joined.text = format!("{}\n{}", last.text, m.text);
            joined.ranges.extend(m.ranges.iter().copied());
            if matches(&joined) {
                *last = joined;
                continue;
            }
        }
        mods.push(m.clone());
    }
    Item { mods, ..item.clone() }
}

/// Stats of the mods tagged Life that body armour can have (RePoE's mod
/// list and bench crafts), and the trade groups they come in: e explicit,
/// f fractured, i implicit (synthesis, eldritch), c crafted. Hybrid mods'
/// other stats ("#% increased Armour") aren't here: other mods have them too.
const LIFE_STATS: &[(&str, &str)] = &[
    ("+# to maximum Life", "efic"),
    ("#% increased maximum Life", "efic"),
    ("Regenerate # Life per second", "efi"),
    ("Regenerate #% of Life per second", "efi"),
    ("#% increased Life Recovery rate", "efi"),
    ("#% increased Life Reservation Efficiency of Skills", "ef"),
    ("#% of Damage taken Recouped as Life", "efi"),
    ("Every # seconds, Regenerate #% of Life over one second", "ef"),
    ("Recover #% of Life on Kill", "ef"),
    ("#% of Damage is taken from Mana before Life", "ef"),
    ("#% of Damage is taken from Mana before Life while Focused", "efc"),
    ("#% of Evasion Rating is Regenerated as Life per second while Focused", "efc"),
    ("Minions have #% increased maximum Life", "ef"),
];

/// "No life modifiers", unticked: a body armour without a mod tagged Life,
/// searched as one with none of `LIFE_STATS` (a passive mastery rewards
/// such armour). Only from advanced copy, whose headers carry the tags.
fn no_life_filter(item: &Item, data: &TradeData) -> Option<Filter> {
    if item.class != "Body Armours" || !item.advanced || item.mods.iter().any(|m| m.tags.iter().any(|t| t == "Life")) {
        return None;
    }
    let mut ids = Vec::new();
    for (text, groups) in LIFE_STATS {
        for (c, group) in [('e', "explicit"), ('f', "fractured"), ('i', "implicit"), ('c', "crafted")] {
            if !groups.contains(c) {
                continue;
            }
            // A stat with a fixed number ("Every 4 seconds") is listed with it.
            let numbered = data.numbered.get(&(group.to_string(), text.to_string())).into_iter().flatten();
            let found = data.stat_ids(group, text).into_iter().flatten();
            let numbered = numbered.filter_map(|(t, _)| data.stats.get(&(group.to_string(), t.clone())).cloned());
            for id in found.chain(numbered) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    (!ids.is_empty()).then(|| Filter {
        label: "No life modifiers".into(),
        target: Target::NoneOf(ids),
        min: None,
        slider: None,
        draw_slider: false,
        step: 1.0,
        max_roll: false,
        default_on: false,
        section: Section::Total,
        tier: None,
        increased: None,
        max: None,
        roll: None,
    })
}

/// One fetched listing: amount, currency id, currency name, chaos value,
/// minutes since it was listed, the item.
type Single = (f64, String, String, Option<f64>, Option<i64>, Preview, Values);

/// A listed item's values for the search's enabled filters, by filter index,
/// as shown in the layer's columns: "158", "81%".
pub type Values = Vec<(usize, String)>;

/// Mod lists of a listed item, and their keys in `extended.hashes`.
const MOD_LISTS: [(&str, &str); 9] = [
    ("enchantMods", "enchant"),
    // PoE2's.
    ("runeMods", "rune"),
    ("desecratedMods", "desecrated"),
    ("implicitMods", "implicit"),
    ("fracturedMods", "fractured"),
    ("explicitMods", "explicit"),
    ("craftedMods", "crafted"),
    ("scourgeMods", "scourge"),
    ("pseudoMods", "pseudo"),
];

/// What a listed item has of a filter's stat or property, as shown: "158",
/// "81%", "7.5" (an "Adds # to #" mod's average). `None` for flags and
/// options (every listing has them) and what the reply doesn't say.
fn listing_value(item: &Value, target: &Target) -> Option<String> {
    let (value, percent) = match target {
        Target::Stat(id) => stat_value(item, id)?,
        Target::AnyStat(ids) => ids.iter().find_map(|id| stat_value(item, id))?,
        Target::Property(group, key) => property_value(item, group, key)?,
        Target::StatOption(..) | Target::Flag(..) | Target::NoneOf(..) => return None,
    };
    Some(format!("{}{}", trim_number(value), if percent { "%" } else { "" }))
}

/// A stat's value on a listed item: summed over its mods with that stat
/// (an implicit and an explicit of the same stat are separate trade stats,
/// but a pseudo total isn't), and whether it's a percentage. Mods name
/// their stat ("hash": "stat.explicit.stat_3299347043"); plain-string mods
/// (the older format) by their place in `extended.hashes`.
fn stat_value(item: &Value, id: &str) -> Option<(f64, bool)> {
    let mut found: Option<(f64, bool)> = None;
    for (key, kind) in MOD_LISTS {
        let hashes = item["extended"]["hashes"][kind].as_array();
        for (i, m) in item[key].as_array().into_iter().flatten().enumerate() {
            let hash =
                m["hash"].as_str().map(|h| h.trim_start_matches("stat.")).or_else(|| hashes?.get(i)?[0].as_str());
            if hash != Some(id) {
                continue;
            }
            let Some(text) = m.as_str().or(m["description"].as_str()) else { continue };
            let Some((value, percent)) = text_value(text) else { continue };
            found = Some(found.map_or((value, percent), |(sum, p)| (sum + value, p || percent)));
        }
    }
    found
}

/// A mod line's value as the trade site compares it: its number, or the
/// average of "Adds # to #"; and whether it's a percentage.
fn text_value(text: &str) -> Option<(f64, bool)> {
    let (template, values) = templatize(text.lines().next()?);
    let value = match values.as_slice() {
        [lo, hi] if template.contains("# to #") => (lo + hi) / 2.0,
        [v, ..] => *v,
        [] => return None,
    };
    Some((value, template.contains("#%")))
}

/// An item property's value on a listed item (weapon DPS, armour, item
/// level, a map's quantity, ...), and whether it's a percentage.
fn property_value(item: &Value, group: &str, key: &str) -> Option<(f64, bool)> {
    let prop = |name: &str| -> Option<f64> {
        let p = item["properties"].as_array()?.iter().find(|p| p["name"] == name)?;
        let text = p["values"][0][0].as_str()?;
        templatize(text).1.first().copied()
    };
    let ext = &item["extended"];
    let number = match (group, key) {
        ("weapon_filters", "pdps" | "edps") => (ext[key].as_f64()?.round(), false),
        ("weapon_filters", "aps") => (prop("Attacks per Second")?, false),
        ("weapon_filters", "crit") => (prop("Critical Strike Chance").or_else(|| prop("Critical Hit Chance"))?, true),
        ("armour_filters", "ar") => (ext["ar"].as_f64().or_else(|| prop("Armour"))?, false),
        ("armour_filters", "ev") => (ext["ev"].as_f64().or_else(|| prop("Evasion Rating"))?, false),
        ("armour_filters", "es") => (ext["es"].as_f64().or_else(|| prop("Energy Shield"))?, false),
        ("armour_filters", "ward") => (ext["ward"].as_f64().or_else(|| prop("Ward"))?, false),
        ("armour_filters", "block") => (prop("Chance to Block").or_else(|| prop("Block chance"))?, true),
        ("equipment_filters", "spirit") => (prop("Spirit")?, false),
        ("misc_filters", "ilvl") => (item["ilvl"].as_f64()?, false),
        ("map_filters", "map_tier") => (prop("Map Tier")?, false),
        ("map_filters", "map_iiq") => (prop("Item Quantity")?, true),
        ("map_filters", "map_iir") => (prop("Item Rarity")?, true),
        ("map_filters", "map_packsize") => (prop("Monster Pack Size")?, true),
        ("misc_filters", "stored_experience") => {
            let p = item["properties"].as_array()?.iter().find(|p| p["name"] == "Stored Experience")?;
            let text = p["values"][0][0].as_str()?;
            (text.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()?, false)
        }
        _ => return None,
    };
    Some(number)
}

/// A short name for a filter, over its column of the listings' values:
/// "+172 total maximum Life" -> "Life", "+86% total Elemental Resistance"
/// -> "Ele res", "Physical DPS 312" -> "pDPS". Long ones the layer cuts.
pub fn column_name(label: &str) -> String {
    const PHRASES: [(&str, &str); 18] = [
        ("Physical DPS", "pDPS"),
        ("Elemental DPS", "eDPS"),
        ("Attacks per second", "APS"),
        ("Critical strike chance", "Crit"),
        ("Critical Strike Chance", "Crit"),
        ("Critical hit chance", "Crit"),
        ("Chance to block", "Block"),
        ("Item level", "ilvl"),
        ("Energy Shield", "ES"),
        ("Energy shield", "ES"),
        ("Evasion Rating", "Evasion"),
        ("Movement Speed", "Move speed"),
        ("Rune sockets", "Sockets"),
        ("Attack Speed", "Atk speed"),
        ("Rarity of Items found", "Rarity"),
        ("Memory Strands", "Strands"),
        ("Gem level", "Level"),
        ("Stored experience", "XP"),
    ];
    const DROP: [&str; 13] =
        ["to", "total", "maximum", "increased", "reduced", "of", "the", "a", "an", "Adds", "and", "with", "+"];
    const SHORT: [(&str, &str); 10] = [
        ("Elemental", "Ele"),
        ("Resistance", "res"),
        ("Resistances", "res"),
        ("Strength", "Str"),
        ("Dexterity", "Dex"),
        ("Intelligence", "Int"),
        ("Damage", "dmg"),
        ("Physical", "Phys"),
        ("Multiplier", "multi"),
        ("Attributes", "attrs"),
    ];
    // "(at 20% quality)" and the like say how, not what.
    let mut text = label.split(" (").next().unwrap_or(label).to_string();
    for (long, short) in PHRASES {
        text = text.replace(long, short);
    }
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit() || c == '#') && !DROP.contains(w))
        .map(|w| SHORT.iter().find(|(long, _)| *long == w).map_or(w, |(_, short)| short))
        .collect();
    let mut name = words.join(" ");
    // Sentence case, but not "pDPS" or "ilvl".
    if let (Some(first), false) = (name.get(..1), ["pDPS", "eDPS", "ilvl"].contains(&name.as_str())) {
        name = first.to_uppercase() + &name[1..];
    }
    if name.is_empty() { label.to_string() } else { name }
}

/// A listed item for the hover preview, as (kind, text, affix tier) lines:
/// "icon" (its image's URL on web.poecdn.com), "cells" ("2 2": its size in
/// inventory cells), "rarity" (normal/magic/rare/unique), "name", "base",
/// "ilvl", "sockets" ("R-G-B B"), "influence" (one per influence), "dps",
/// "enchant", "implicit", then the mods: "fractured", "explicit", "crafted",
/// "veiled" and others the trade site marks by domain (e.g. "desecrated"),
/// then "mirrored", "corrupted", "unidentified".
pub type Preview = Vec<(&'static str, String, Option<u32>)>;

/// Mod domains the preview colours; others count as plain explicit mods.
const MOD_DOMAINS: [&str; 8] =
    ["fractured", "crafted", "veiled", "desecrated", "scourge", "crucible", "enchant", "implicit"];

fn item_preview(item: &Value) -> Preview {
    let mut out: Preview = Vec::new();
    let rarity = match item["frameType"].as_u64() {
        Some(1) => "magic",
        Some(2) => "rare",
        Some(3) => "unique",
        Some(4) => "gem",
        _ => "normal",
    };
    if let Some(icon) = item["icon"].as_str().filter(|i| i.starts_with(ICON_CDN)) {
        out.push(("icon", icon.to_string(), None));
        let (w, h) = (item["w"].as_u64().unwrap_or(1), item["h"].as_u64().unwrap_or(1));
        out.push(("cells", format!("{w} {h}"), None));
    }
    out.push(("rarity", rarity.to_string(), None));
    if let Some(tag) = gem_tag(item) {
        out.push(("gem", tag, None));
    }
    for (kind, key) in [("name", "name"), ("base", "typeLine")] {
        if let Some(text) = item[key].as_str().filter(|t| !t.is_empty()) {
            out.push((kind, strip_markup(text), None));
        }
    }
    if let Some(ilvl) = item["ilvl"].as_u64() {
        out.push(("ilvl", ilvl.to_string(), None));
    }
    if let Some(sockets) = sockets_text(&item["sockets"]) {
        out.push(("sockets", sockets, None));
    }
    if let Some(influences) = item["influences"].as_object() {
        for (name, _) in influences.iter().filter(|(_, on)| on.as_bool() == Some(true)) {
            out.push(("influence", name.clone(), None));
        }
    }
    if let Some(quality) = quality_line(item) {
        out.push(("quality", quality, None));
    }
    let ext = &item["extended"];
    let dps: Vec<String> = [("pDPS", "pdps"), ("eDPS", "edps")]
        .iter()
        .filter_map(|(label, key)| ext[*key].as_f64().filter(|v| *v > 0.0).map(|v| format!("{label} {v:.0}")))
        .collect();
    if !dps.is_empty() {
        out.push(("dps", dps.join(" · "), None));
    }
    for (kind, key) in [
        ("enchant", "enchantMods"),
        // PoE2's mods from runes, shown like enchants (as in the panel's
        // filters), and its desecrated mods like explicit ones.
        ("enchant", "runeMods"),
        ("implicit", "implicitMods"),
        ("fractured", "fracturedMods"),
        ("explicit", "explicitMods"),
        ("explicit", "desecratedMods"),
        ("crafted", "craftedMods"),
        ("veiled", "veiledMods"),
    ] {
        // Plain strings, or (lately) objects with the text as "description",
        // the kind as "domain" (fractured and crafted mods are now among
        // explicitMods), and the affixes behind it, with tiers like "P7"
        // (prefix tier 7).
        for m in item[key].as_array().into_iter().flatten() {
            let Some(text) = m.as_str().or(m["description"].as_str()) else { continue };
            let kind =
                m["domain"].as_str().and_then(|d| MOD_DOMAINS.iter().find(|k| **k == d).copied()).unwrap_or(kind);
            let affixes = m["mods"].as_array().into_iter().flatten();
            let tier = affixes.clone().filter_map(|a| affix_tier(a["tier"].as_str()?)).min();
            // Veiled mods' text is a placeholder ("Suffix02").
            if kind == "veiled" {
                let side = affixes.filter_map(|a| a["tier"].as_str()).next().unwrap_or("");
                let text = if side.starts_with('P') { "Veiled Prefix" } else { "Veiled Suffix" };
                out.push((kind, text.to_string(), None));
                continue;
            }
            // Some mods span lines ("…\nhave 20% reduced …"); the tier goes on the first.
            for (i, line) in strip_markup(text).lines().enumerate() {
                out.push((kind, line.to_string(), if i == 0 { tier } else { None }));
            }
        }
    }
    if item["duplicated"].as_bool() == Some(true) {
        out.push(("mirrored", "Mirrored".to_string(), None));
    }
    if item["identified"].as_bool() == Some(false) {
        out.push(("unidentified", "Unidentified".to_string(), None));
    }
    if item["corrupted"].as_bool() == Some(true) {
        out.push(("corrupted", "Corrupted".to_string(), None));
    }
    out
}

/// A listed gem's level and quality: "Level 20 · 23%" (shown on its row,
/// like APT's columns).
/// A listing's quality as the game shows it: "Quality: +20%", or a
/// catalyst's on jewellery, "Quality (Resistance Modifiers): +20%". That
/// tells a mod the catalyst raised past its range from a better roll. Gems
/// show theirs in `gem_tag`.
fn quality_line(item: &Value) -> Option<String> {
    if item["frameType"].as_u64() == Some(4) {
        return None;
    }
    let prop = item["properties"].as_array()?.iter().find(|p| {
        p["name"].as_str().is_some_and(|n| strip_markup(n) == "Quality" || strip_markup(n).starts_with("Quality ("))
    })?;
    let value = prop["values"][0][0].as_str().filter(|v| !v.is_empty() && *v != "+0%")?;
    Some(format!("{}: {value}", strip_markup(prop["name"].as_str()?)))
}

fn gem_tag(item: &Value) -> Option<String> {
    if item["frameType"].as_u64() != Some(4) {
        return None;
    }
    let prop = |name: &str| -> Option<String> {
        let p = item["properties"].as_array()?.iter().find(|p| p["name"] == name)?;
        let v = p["values"][0][0].as_str()?;
        Some(
            v.chars().take_while(|c| c.is_ascii_digit() || *c == '+' || *c == '%').collect::<String>().replace('+', ""),
        )
    };
    let level = prop("Level").map(|l| format!("Level {l}"));
    let quality = prop("Quality").filter(|q| q != "0%" && !q.is_empty());
    let parts: Vec<String> = level.into_iter().chain(quality).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The `icon` id of the socket sprite sheet.
pub const SOCKET_SPRITE: &str = "sockets";

/// pathofexile.com's stylesheets name the socket sprite with a key that
/// changes with the site, so it's looked up there: the home page links them
/// (it isn't behind Cloudflare's check, unlike the trade pages).
fn socket_sprite_url() -> Result<String, String> {
    let get = |url: &str| -> Result<String, String> {
        let resp = ureq::get(url)
            .set("User-Agent", ninja::USER_AGENT)
            .timeout(Duration::from_secs(20))
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        let mut text = String::new();
        resp.into_reader().take(8 << 20).read_to_string(&mut text).map_err(|e| e.to_string())?;
        Ok(text)
    };
    let home = get("https://www.pathofexile.com/")?;
    for css in stylesheet_links(&home) {
        if let Some(path) = socket_sprite_path(&get(&css)?) {
            return Ok(format!("https://web.poecdn.com{path}"));
        }
    }
    Err("no socket images in pathofexile.com's stylesheets".into())
}

/// The web.poecdn.com stylesheets a page links.
fn stylesheet_links(html: &str) -> Vec<String> {
    html.split(['"', '\''])
        .filter(|s| s.starts_with(ICON_CDN) && s.split('?').next().unwrap_or("").ends_with(".css"))
        .map(str::to_string)
        .collect()
}

/// "/protected/image/gen/socket.png?v=…&key=…" in a stylesheet.
fn socket_sprite_path(css: &str) -> Option<&str> {
    let start = css.find("/protected/image/gen/socket.png")?;
    let rest = &css[start..];
    Some(&rest[..rest.find([')', '"', '\'', ' ']).unwrap_or(rest.len())])
}

/// 64-bit FNV-1a: a stable file name for a cached item image.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// Where listed items' images live; the daemon fetches only from here.
pub const ICON_CDN: &str = "https://web.poecdn.com/";

/// Sockets as PoE writes them in item text: linked ones joined by "-",
/// groups apart ("R-G-B B"). None without sockets.
fn sockets_text(sockets: &Value) -> Option<String> {
    // PoE2's rune sockets have a type and no colour or links: in words.
    let list = sockets.as_array()?;
    if !list.is_empty() && list.iter().all(|s| s["sColour"].is_null() && s["type"].is_string()) {
        let n = list.len();
        return Some(format!("{n} rune socket{}", if n == 1 { "" } else { "s" }));
    }
    let mut out = String::new();
    let mut group = None;
    for s in list {
        let colour = s["sColour"].as_str().unwrap_or("?");
        let g = s["group"].as_u64();
        if group.is_some() {
            out.push(if g == group { '-' } else { ' ' });
        }
        group = g;
        out.push_str(colour);
    }
    (!out.is_empty()).then_some(out)
}

/// The trade site's affix tier: "P7" (prefix) or "S4" (suffix) -> 7 / 4.
/// Others (crafted "R1" ranks, uniques' empty tier) aren't tiers.
fn affix_tier(tier: &str) -> Option<u32> {
    tier.strip_prefix('P').or_else(|| tier.strip_prefix('S'))?.parse().ok()
}

/// "[Strength|Strength]" / "[Life]" link markup -> plain text.
fn strip_markup(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let Some(len) = rest[start..].find(']') else { break };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 1..start + len];
        out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// A filter the layer wants searched, with the bounds it chose (no minimum:
/// the filter's default; no maximum: unbounded).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice {
    pub index: usize,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// The filters to search an enchanted item (a Heist Enchanted Armament) by,
/// from the Base item preset's: its type and its enchants, but of those
/// only the ones with a number (the magnitude or effect, what it's worth)
/// if it has any. "Has no Green Sockets" too would leave next to nothing.
pub fn enchanted_choices(filters: &[Filter]) -> Vec<Choice> {
    let enchant = |f: &Filter| matches!(&f.target, Target::Stat(id) if id.starts_with("enchant."));
    let numbered = filters.iter().any(|f| enchant(f) && f.min.is_some());
    filters
        .iter()
        .enumerate()
        .filter(|(_, f)| f.default_on && !(numbered && enchant(f) && f.min.is_none()))
        .map(|(index, f)| Choice { index, min: f.min, max: None })
        .collect()
}

/// Minimum and maximum of a stat or property filter.
type Bounds = (Option<f64>, Option<f64>);

fn bounds_json((min, max): Bounds) -> Value {
    // Some filters (links, sockets) refuse "6.0".
    let number = |n: f64| if n.fract() == 0.0 && n.abs() < 1e15 { json!(n as i64) } else { json!(n) };
    let mut v = json!({});
    if let Some(min) = min {
        v["min"] = number(min);
    }
    if let Some(max) = max {
        v["max"] = number(max);
    }
    v
}

/// A transfigured gem's type for the query: the trade site knows "Arc of
/// Oscillating" as Arc with a discriminator.
fn transfigured_type(item: &Item, data: &TradeData) -> Option<Value> {
    if item.rarity != "Gem" {
        return None;
    }
    let (base, disc) = data.transfigured.get(&item.base)?;
    Some(json!({ "option": base, "discriminator": disc }))
}

/// A Mercenary Warrant's type for the query: the trade site knows each
/// build as its own warrant, "Mercenary Warrant (Chaplain of the Well)".
fn warrant_type(item: &Item, data: &TradeData) -> Option<Value> {
    let (base, disc) = data.warrants.get(&item.warrant.as_ref()?.build)?;
    Some(json!({ "option": base, "discriminator": disc }))
}

/// A Mercenary Warrant's skills and supports, unticked: the warrant's build
/// alone is the search, and each one ticked narrows it.
fn warrant_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let Some(warrant) = &item.warrant else { return Vec::new() };
    let mut seen = Vec::new();
    warrant
        .skills
        .iter()
        .filter_map(|skill| {
            let id = data.stats.get(&("mercenary".to_string(), skill.clone()))?;
            // A support on two skills is one stat.
            if seen.contains(&id) {
                return None;
            }
            seen.push(id);
            Some(Filter {
                draw_slider: false,
                min: None,
                slider: None,
                roll: None,
                ..Filter::new(skill.clone(), Target::Stat(id.clone()), 0.0, None, false, Section::Mod)
            })
        })
        .collect()
}

/// `chosen[i]`: whether filter i is used, and its bounds.
fn query(item: &Item, filters: &[Filter], chosen: &[Option<Bounds>], options: SearchOptions) -> Value {
    let mut stats = Vec::new();
    let unique = item.rarity == "Unique";
    let mut q = json!({
        "status": { "option": options.status.option() },
        "filters": { "type_filters": { "filters": { "rarity": { "option": if unique { "unique" } else { "nonunique" } } } } },
    });
    let mut any_of = Vec::new();
    let mut none_of = Vec::new();
    let stat = |id: &str, bounds| match bounds {
        (None, None) => json!({ "id": id }),
        _ => json!({ "id": id, "value": bounds_json(bounds) }),
    };
    for (f, choice) in filters.iter().zip(chosen) {
        let Some(bounds) = *choice else { continue };
        match &f.target {
            Target::Stat(id) => stats.push(stat(id, bounds)),
            Target::AnyStat(ids) => any_of.push(json!({
                "type": "count",
                "value": { "min": 1 },
                "filters": ids.iter().map(|id| stat(id, bounds)).collect::<Vec<_>>(),
            })),
            Target::StatOption(id, option) => stats.push(json!({ "id": id, "value": { "option": option } })),
            Target::Property(group, key) => q["filters"][*group]["filters"][*key] = bounds_json(bounds),
            Target::Flag(group, key) => q["filters"][*group]["filters"][*key] = json!({ "option": "true" }),
            Target::NoneOf(ids) => none_of.extend(ids.iter().map(|id| json!({ "id": id }))),
        }
    }
    let not = (!none_of.is_empty()).then(|| json!({ "type": "not", "filters": none_of }));
    q["stats"] =
        Value::Array([json!({ "type": "and", "filters": stats })].into_iter().chain(any_of).chain(not).collect());
    if unique {
        q["name"] = json!(item.name);
        q["type"] = json!(item.base);
    } else {
        match category(item) {
            Some((c, _)) if !options.base_only && options.preset == Preset::Normal => {
                q["filters"]["type_filters"]["filters"]["category"] = json!({ "option": c })
            }
            _ => q["type"] = json!(trade_base(item)),
        }
    }
    if options.six_link {
        q["filters"]["socket_filters"]["filters"]["links"] = json!({ "min": 6 });
    }
    if options.unidentified {
        q["filters"]["misc_filters"]["filters"]["identified"] = json!({ "option": "false" });
    }
    match options.corrupted {
        Corrupted::Any => {}
        Corrupted::No => q["filters"]["misc_filters"]["filters"]["corrupted"] = json!({ "option": "false" }),
        Corrupted::Yes => q["filters"]["misc_filters"]["filters"]["corrupted"] = json!({ "option": "true" }),
    }
    if game::current() == game::Game::Poe2 {
        poe2_filters(&mut q["filters"]);
    }
    json!({ "query": q, "sort": { "price": "asc" } })
}

/// The filters for an item, which of them are used (with their bounds),
/// and the search's body: `enabled` ticked, the rest disabled.
fn search_body(
    data: &TradeData,
    item: &Item,
    enabled: &[Choice],
    options: SearchOptions,
) -> (Vec<Filter>, Vec<Option<Bounds>>, Value) {
    let filters = build_filters(item, data, options.preset);
    let chosen: Vec<Option<Bounds>> = filters
        .iter()
        .enumerate()
        .map(|(i, f)| enabled.iter().find(|c| c.index == i).map(|c| (c.min.or(f.min), c.max)))
        .collect();
    let mut body = query(item, &filters, &chosen, options);
    add_unticked(&mut body, &filters, &chosen);
    if let Some(t) = transfigured_type(item, data).or_else(|| warrant_type(item, data)) {
        body["query"]["type"] = t;
    }
    (filters, chosen, body)
}

/// The trade site's page for a search body, with the query in its address.
fn site_url(body: &Value, league: &str) -> String {
    format!("{}/{}?q={}", game::current().trade_site(), ninja::encode(league), ninja::encode(&body.to_string()))
}

/// Adds the unticked modifiers to `body` (a `query`) as disabled stat
/// filters, with their default bounds: they don't change what's found, but
/// "Open on trade site" shows every modifier, unticked ones unticked, for
/// the user to tick there. Item properties (DPS, links) can't be disabled
/// one by one, and the "no life" group says nothing on the trade site
/// unticked, so those stay out.
fn add_unticked(body: &mut Value, filters: &[Filter], chosen: &[Option<Bounds>]) {
    let stat = |id: &str, bounds: Bounds| match bounds {
        (None, None) => json!({ "id": id, "disabled": true }),
        _ => json!({ "id": id, "value": bounds_json(bounds), "disabled": true }),
    };
    let stats = &mut body["query"]["stats"];
    for (f, choice) in filters.iter().zip(chosen) {
        if choice.is_some() {
            continue;
        }
        let bounds = (f.min, f.max);
        match &f.target {
            Target::Stat(id) => push_json(&mut stats[0]["filters"], stat(id, bounds)),
            Target::StatOption(id, option) => push_json(
                &mut stats[0]["filters"],
                json!({ "id": id, "value": { "option": option }, "disabled": true }),
            ),
            Target::AnyStat(ids) => push_json(
                stats,
                json!({
                    "type": "count",
                    "value": { "min": 1 },
                    "filters": ids.iter().map(|id| stat(id, bounds)).collect::<Vec<_>>(),
                    "disabled": true,
                }),
            ),
            Target::Property(..) | Target::Flag(..) | Target::NoneOf(_) => {}
        }
    }
}

fn push_json(array: &mut Value, v: Value) {
    if let Some(a) = array.as_array_mut() {
        a.push(v);
    }
}

/// PoE2's trade site has the filters of /api/trade2/data/filters: weapon
/// and armour properties together in `equipment_filters`, item level and
/// quality in `type_filters`, no links. Moves PoE1's filters where PoE2
/// has them, and drops what PoE2 doesn't have (it refuses the whole search
/// over an unknown filter).
fn poe2_filters(filters: &mut Value) {
    const POE2: &[(&str, &[&str])] = &[
        ("status_filters", &["status"]),
        ("type_filters", &["category", "rarity", "ilvl", "quality"]),
        (
            "equipment_filters",
            &[
                "damage",
                "aps",
                "crit",
                "dps",
                "pdps",
                "edps",
                "reload_time",
                "ar",
                "ev",
                "es",
                "ward",
                "block",
                "spirit",
                "rune_sockets",
                "total_augment_sockets",
            ],
        ),
        ("req_filters", &["lvl", "str", "dex", "int"]),
        (
            "map_filters",
            &[
                "map_tier",
                "map_packsize",
                "map_magic_monsters",
                "map_iir",
                "map_rare_monsters",
                "map_revives",
                "map_bonus",
                "map_gold",
                "map_experience",
                "ultimatum_hint",
            ],
        ),
        (
            "misc_filters",
            &[
                "gem_level",
                "gem_sockets",
                "area_level",
                "stack_size",
                "identified",
                "fractured_item",
                "corrupted",
                "sanctified",
                "twice_corrupted",
                "mutated",
                "veiled",
                "desecrated",
                "crafted",
                "foreseeing",
                "mirrored",
                "sanctum_gold",
                "unidentified_tier",
            ],
        ),
        ("trade_filters", &["account", "collapse", "indexed", "sale_type", "fee", "price"]),
    ];
    let Some(groups) = filters.as_object_mut() else { return };
    let mut moved = serde_json::Map::new();
    for (group, body) in std::mem::take(groups) {
        let Value::Object(mut body) = body else { continue };
        let Some(Value::Object(inner)) = body.remove("filters") else { continue };
        for (key, value) in inner {
            let to = match (group.as_str(), key.as_str()) {
                ("weapon_filters" | "armour_filters", _) => "equipment_filters",
                ("misc_filters", "ilvl" | "quality") => "type_filters",
                (g, _) => g,
            };
            if !POE2.iter().any(|(g, keys)| *g == to && keys.contains(&key.as_str())) {
                eprintln!("trade: PoE2 has no {group}.{key} filter; searching without it");
                continue;
            }
            let entry = moved.entry(to).or_insert_with(|| json!({ "filters": {} }));
            entry["filters"][key] = value;
        }
    }
    *groups = moved;
}

// ---- rate limits -----------------------------------------------------------

/// Follows GGG's X-Rate-Limit-* headers per endpoint and refuses requests
/// that would hit a limit: going over gets the IP timed out.
#[derive(Default)]
struct RateLimiter {
    /// endpoint -> (max hits, window) from the latest response
    rules: HashMap<&'static str, Vec<(u32, Duration)>>,
    history: HashMap<&'static str, VecDeque<Instant>>,
    blocked_until: Option<Instant>,
}

impl RateLimiter {
    fn check(&mut self, endpoint: &'static str) -> Result<(), String> {
        let now = Instant::now();
        if let Some(until) = self.blocked_until.filter(|t| *t > now) {
            return Err(format!("Trade API rate limit: try again in {}s", (until - now).as_secs() + 1));
        }
        // Until a reply says what the limits are: a search or a fetch every
        // 5 s, like Awakened PoE Trade. The first reply reveals the rest.
        let cautious = [(2, Duration::from_secs(5))];
        let rules: &[(u32, Duration)] = match self.rules.get(endpoint) {
            Some(rules) => rules,
            None if matches!(endpoint, "search" | "fetch") => &cautious,
            None => return Ok(()),
        };
        let none = VecDeque::new();
        let history = self.history.get(endpoint).unwrap_or(&none);
        for &(max, window) in rules {
            let recent: Vec<&Instant> = history.iter().filter(|t| now.duration_since(**t) < window).collect();
            // Keep one request of headroom.
            if recent.len() as u32 + 1 >= max {
                let oldest = recent.iter().min().unwrap();
                let wait = window.saturating_sub(now.duration_since(**oldest));
                return Err(format!("Trade API rate limit: try again in {}s", wait.as_secs() + 1));
            }
        }
        Ok(())
    }

    fn record(&mut self, endpoint: &'static str, resp: &HttpResponse) {
        let now = Instant::now();
        let history = self.history.entry(endpoint).or_default();
        history.push_back(now);
        while history.len() > 200 {
            history.pop_front();
        }

        // "x-rate-limit-rules: Ip" + "x-rate-limit-ip: 8:10:60,15:60:300"
        // (hits:window:penalty) + "x-rate-limit-ip-state: 1:10:0,..."
        // (hits:window:active penalty).
        let mut rules = Vec::new();
        for rule in resp.headers.get("x-rate-limit-rules").map(String::as_str).unwrap_or("").split(',') {
            let rule = rule.trim().to_lowercase();
            if rule.is_empty() {
                continue;
            }
            for part in resp.headers.get(&format!("x-rate-limit-{rule}")).map(String::as_str).unwrap_or("").split(',') {
                let f: Vec<u64> = part.split(':').filter_map(|n| n.parse().ok()).collect();
                if let [max, window, ..] = f[..] {
                    rules.push((max as u32, Duration::from_secs(window)));
                }
            }
            for part in
                resp.headers.get(&format!("x-rate-limit-{rule}-state")).map(String::as_str).unwrap_or("").split(',')
            {
                let f: Vec<u64> = part.split(':').filter_map(|n| n.parse().ok()).collect();
                if let [_, _, penalty, ..] = f[..]
                    && penalty > 0
                {
                    self.block_for(Duration::from_secs(penalty));
                }
            }
        }
        if !rules.is_empty() {
            self.rules.insert(endpoint, rules);
        }
        if resp.status == 429 {
            let secs = resp.headers.get("retry-after").and_then(|s| s.parse().ok()).unwrap_or(60);
            self.block_for(Duration::from_secs(secs));
        }
    }

    fn block_for(&mut self, d: Duration) {
        let until = Instant::now() + d;
        self.blocked_until = Some(self.blocked_until.map_or(until, |t| t.max(until)));
    }
}

#[cfg(test)]
mod tests {
    /// `poe2_filters`' list is the site's.
    #[test]
    fn poe2_filter_list_matches_the_site() {
        let site: Value = serde_json::from_str(include_str!("../fixtures/poe2/trade-filters.json")).unwrap();
        let mut all = json!({});
        for group in site["result"].as_array().unwrap() {
            for f in group["filters"].as_array().unwrap() {
                all[group["id"].as_str().unwrap()]["filters"][f["id"].as_str().unwrap()] = json!({});
            }
        }
        let mut kept = all.clone();
        poe2_filters(&mut kept);
        assert_eq!(kept, all, "a filter the site has is dropped, or the site has new ones");
    }

    #[test]
    fn poe2_filters_move_and_drop() {
        let mut f = json!({
            "type_filters": { "filters": { "rarity": { "option": "nonunique" } } },
            "weapon_filters": { "filters": { "pdps": { "min": 300 } } },
            "armour_filters": { "filters": { "es": { "min": 100 } } },
            "misc_filters": { "filters": { "ilvl": { "min": 82 }, "corrupted": { "option": "false" },
                "synthesised_item": { "option": "true" } } },
            "socket_filters": { "filters": { "links": { "min": 6 } } },
        });
        poe2_filters(&mut f);
        assert_eq!(
            f,
            json!({
                "type_filters": { "filters": { "rarity": { "option": "nonunique" }, "ilvl": { "min": 82 } } },
                "equipment_filters": { "filters": { "pdps": { "min": 300 }, "es": { "min": 100 } } },
                "misc_filters": { "filters": { "corrupted": { "option": "false" } } },
            })
        );
    }

    use super::*;

    #[test]
    fn templates() {
        assert_eq!(templatize("+45 to maximum Life"), ("+# to maximum Life".into(), vec![45.0]));
        assert_eq!(
            templatize("Adds 5 to 10 Physical Damage to Attacks"),
            ("Adds # to # Physical Damage to Attacks".into(), vec![5.0, 10.0])
        );
        assert_eq!(templatize("0.8% of Physical Attack Damage Leeched as Life").1, vec![0.8]);
    }

    #[test]
    fn estimates() {
        let single = |amount: f64, minutes: i64| -> Single {
            (amount, "chaos".into(), "Chaos Orb".into(), Some(amount), Some(minutes), Preview::default(), Values::new())
        };
        let fresh = |prices: &[f64]| prices.iter().map(|&p| single(p, 60)).collect::<Vec<_>>();
        assert_eq!(estimate_index(&[]), None);
        // Few listings: the cheapest, not the median.
        assert_eq!(estimate_index(&fresh(&[45.0, 1731.0, 2164.0, 7574.0])), Some((0, false)));
        // Ten: the middle of the cheapest two, so one lowball doesn't set it.
        assert_eq!(
            estimate_index(&fresh(&[1.0, 50.0, 55.0, 60.0, 60.0, 70.0, 80.0, 90.0, 100.0, 200.0])),
            Some((1, false))
        );
        // Unsold for days: caps the price.
        let mut v = fresh(&[45.0, 1731.0, 2164.0, 7574.0]);
        v[0] = single(45.0, 4 * 24 * 60);
        assert_eq!(estimate_index(&v), Some((0, true)));
        // A stale listing above the pick doesn't matter.
        let mut v = fresh(&[1.0, 50.0, 55.0, 60.0, 60.0, 70.0, 80.0, 90.0, 100.0, 200.0]);
        v[5] = single(70.0, 5 * 24 * 60);
        assert_eq!(estimate_index(&v), Some((1, false)));
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-18T22:01:33Z"), Some(1795039293));
        assert_eq!(parse_utc("nonsense"), None);
    }

    fn test_data() -> TradeData {
        let mut stats = HashMap::new();
        for (text, id) in [
            ("+# total maximum Life", "pseudo.pseudo_total_life"),
            ("+#% total Elemental Resistance", "pseudo.pseudo_total_elemental_resistance"),
            ("+#% total to Chaos Resistance", "pseudo.pseudo_total_chaos_resistance"),
            ("+# total maximum Mana", "pseudo.pseudo_total_mana"),
            ("+# total to Strength", "pseudo.pseudo_total_strength"),
            ("+# total to Dexterity", "pseudo.pseudo_total_dexterity"),
            ("+# total to Intelligence", "pseudo.pseudo_total_intelligence"),
        ] {
            stats.insert(("pseudo".to_string(), text.to_string()), id.to_string());
        }
        // Like GGG's list: rarity is only searchable as a pseudo stat.
        stats.insert(("pseudo".into(), "#% increased Rarity of Items found".into()), "pseudo.rarity".into());
        for (text, id) in [
            ("+# to maximum Life", "explicit.life"),
            ("#% increased Fire Damage", "explicit.fire_damage"),
            ("+#% to Fire Resistance", "explicit.fire_res"),
            ("+# to Intelligence", "explicit.int"),
            ("+#% to all Elemental Resistances", "explicit.all_res"),
            ("#% chance to Freeze, Shock and Ignite", "explicit.fsi"),
            (
                "#% increased Elemental Damage with Hits and Ailments for\neach type of Elemental Ailment on Enemy",
                "explicit.ele_per_ailment",
            ),
        ] {
            stats.insert(("explicit".to_string(), text.to_string()), id.to_string());
        }
        stats.insert(("fractured".into(), "+# to maximum Life".into()), "fractured.life".into());
        stats.insert(("crafted".into(), "Warcries cannot Exert Travel Skills".into()), "crafted.exert".into());
        stats.insert(("explicit".into(), "+# to maximum Energy Shield".into()), "explicit.es".into());
        stats.insert(
            ("implicit".into(), "Non-Vaal Strike Skills target # additional nearby Enemy".into()),
            "implicit.strike".into(),
        );
        stats.insert(("implicit".into(), "#% of Cold Damage Leeched as Life".into()), "implicit.cold_leech".into());
        stats.insert(
            ("implicit".into(), "Curse Enemies with Conductivity on Hit".into()),
            "implicit.conductivity".into(),
        );
        let mut transfigured = HashMap::new();
        transfigured.insert("Arc of Oscillating".to_string(), ("Arc".to_string(), "alt_y".to_string()));
        TradeData {
            stats,
            same_text: HashMap::new(),
            numbered: HashMap::new(),
            currencies: HashMap::new(),
            images: HashMap::new(),
            transfigured,
            warrants: HashMap::new(),
            names: Arc::new(Vec::new()),
            lower_stats: HashMap::new(),
        }
    }

    const RING: &str = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                        +25% to Cold Resistance (implicit)\n--------\n+60 to maximum Life\n+30 to Strength\n\
                        +30% to Fire Resistance\n+20 to Dexterity and Intelligence\n\
                        12% increased Rarity of Items found\n+12% to Cold Resistance (crafted)\n\
                        8% increased Rarity of Items found (crafted)\n";

    #[test]
    fn heist_enchant_read_off_screen() {
        let mut data = test_data();
        let text = "#% increased Explicit Chaos Modifier magnitudes";
        data.stats.insert(("enchant".into(), text.into()), "enchant.stat_3196512240".into());
        data.lower_stats.insert(("enchant".into(), text.to_lowercase()), text.into());
        let (kind, line) = data.canonical_mod("8% INCREASED EXPLICIT CHAOS MODIFIER MAGNITUDES").unwrap();
        assert_eq!(kind, ModKind::Enchant);
        assert_eq!(line, "8% increased Explicit Chaos Modifier magnitudes");
        // The Base item preset searches the type with the enchant ticked.
        let mut m = crate::item::mod_line(&line);
        m.kind = kind;
        let item = Item {
            rarity: "Rare".into(),
            name: Some("Damnation Bane".into()),
            base: "Tenderizer".into(),
            mods: vec![m],
            ..Default::default()
        };
        let filters = build_filters(&item, &data, Preset::Base);
        let enchant = filters.iter().find(|f| f.target == Target::Stat("enchant.stat_3196512240".into()));
        assert!(enchant.is_some_and(|f| f.default_on), "{:?}", filters.iter().map(|f| &f.label).collect::<Vec<_>>());
        // Property lines aren't mods.
        assert_eq!(data.canonical_mod("PHYSICAL DAMAGE: 112-157"), None);
        // With a socket colour enchant too, only the numbered one counts.
        let sockets = "Has no Green Sockets";
        data.stats.insert(("enchant".into(), sockets.into()), "enchant.stat_no_green".into());
        let mut no_green = crate::item::mod_line(sockets);
        no_green.kind = ModKind::Enchant;
        let item = Item { mods: vec![item.mods[0].clone(), no_green], ..item };
        let filters = build_filters(&item, &data, Preset::Base);
        let chosen: Vec<&Target> = enchanted_choices(&filters).iter().map(|c| &filters[c.index].target).collect();
        assert!(chosen.contains(&&Target::Stat("enchant.stat_3196512240".into())), "{chosen:?}");
        assert!(!chosen.contains(&&Target::Stat("enchant.stat_no_green".into())), "{chosen:?}");
    }

    #[test]
    fn corruption_defaults() {
        let rare = crate::item::parse(crate::item::tests::FRACTURED_CROWN).unwrap();
        let rare_c = crate::item::parse(&format!("{}Corrupted\n", crate::item::tests::FRACTURED_CROWN)).unwrap();
        assert_eq!(default_corrupted(&rare), Corrupted::No);
        assert_eq!(default_corrupted(&rare_c), Corrupted::Yes);
        let mut unique = crate::item::parse(crate::item::tests::FRACTURED_CROWN).unwrap();
        unique.rarity = "Unique".into();
        assert_eq!(default_corrupted(&unique), Corrupted::No);
        unique.corrupted = true;
        assert_eq!(default_corrupted(&unique), Corrupted::Yes);
    }

    #[test]
    fn apt_like_defaults() {
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.min, f.default_on)).collect();
        assert_eq!(
            got,
            vec![
                // 60 life + 30 str / 2
                ("+75 total maximum Life", Some(67.0), true),
                // 25 implicit + 30 + 12 crafted
                ("+67% total Elemental Resistance", Some(60.0), true),
                ("+30 total to Strength", Some(27.0), false),
                ("+20 total to Dexterity", Some(18.0), false),
                ("+20 total to Intelligence", Some(18.0), false),
                // the crafted rarity mod isn't listed
                ("12% increased Rarity of Items found", Some(10.0), false),
            ]
        );
    }

    /// A crafted mod without a number is listed, unticked, as a crafted stat
    /// with nothing to slide.
    #[test]
    fn crafted_mod_without_value() {
        let text = "Item Class: Belts\nRarity: Rare\nGloom Snare\nLeather Belt\n--------\nRequirements:\n\
                    Level: 57\n--------\nItem Level: 77\n--------\n{ Implicit Modifier \u{2014} Life }\n\
                    +30(25-40) to maximum Life\n--------\n\
                    { Prefix Modifier \"Robust\" (Tier: 5) \u{2014} Life }\n+78(70-84) to maximum Life\n\
                    { Prefix Modifier \"Shining\" (Tier: 12) \u{2014} Defences, Energy Shield }\n\
                    +1(1-3) to maximum Energy Shield\n\
                    { Suffix Modifier \"of the Ice\" (Tier: 2) \u{2014} Elemental, Cold, Resistance }\n\
                    +43(42-45)% to Cold Resistance\n\
                    { Suffix Modifier \"of the Magma\" (Tier: 2) \u{2014} Elemental, Fire, Resistance }\n\
                    +45(42-45)% to Fire Resistance\n\
                    { Master Crafted Suffix Modifier \"of Control\" }\n\
                    Warcries cannot Exert Travel Skills \u{2014} Unscalable Value\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(
            got,
            vec![
                ("+108 total maximum Life", true, Section::Total),
                ("+88% total Elemental Resistance", true, Section::Total),
                ("+1 to maximum Energy Shield", false, Section::Mod),
                ("Warcries cannot Exert Travel Skills", false, Section::Crafted),
            ]
        );
        let exert = &filters[3];
        assert_eq!(exert.target, Target::Stat("crafted.exert".into()));
        assert_eq!((exert.min, exert.slider, exert.roll), (None, None, None));
    }

    const WARRANT: &str = "Item Class: Map Fragments\nRarity: Normal\nMercenary Warrant\n--------\n\
        Tiberius, Champion of Kulemak\n--------\nBuild: Chaplain of the Well\nMercenary Level: 83\n--------\n\
        Abyssal Spray\nGreater Pierce (Tier: 3)\nCritical Chance (Tier: 2)\n--------\nPower of the Depths\n\
        --------\nVaal Smite\nGreater Cooldown Recovery (Tier: 3)\n--------\nFlame Dash\n\
        Greater Cooldown Recovery (Tier: 3)\n--------\nRight click this item to view Mercenary details.\n\
        Open a portal to the Proving Grounds by using this item in a personal Map Device.\n";

    #[test]
    fn warrant_by_build_skills_and_supports() {
        let data = real_data();
        let item = crate::item::parse(WARRANT).unwrap();
        let warrant = item.warrant.as_ref().unwrap();
        assert_eq!((warrant.build.as_str(), warrant.level), ("Chaplain of the Well", Some(83)));
        assert_eq!(warrant.skills[1], "Greater Pierce (Tier 3)");
        assert!(searchable(&item));
        let filters = build_filters(&item, &data, Preset::Normal);
        // Only what the trade site lists (the fixture's subset), each once,
        // unticked.
        let shown: Vec<(&str, bool)> = filters.iter().map(|f| (f.label.as_str(), f.default_on)).collect();
        assert_eq!(
            shown,
            vec![
                ("Abyssal Spray", false),
                ("Greater Pierce (Tier 3)", false),
                ("Critical Chance (Tier 2)", false),
                ("Greater Cooldown Recovery (Tier 3)", false),
                ("Flame Dash", false),
            ]
        );
        assert!(filters.iter().all(|f| matches!(&f.target, Target::Stat(id) if id.starts_with("mercenary."))));
        assert_eq!(
            warrant_type(&item, &data),
            Some(json!({ "option": "AbyssalTemplar", "discriminator": "mercenary_warrant" }))
        );
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, f.max))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        let Target::Stat(pierce) = &filters[1].target else { unreachable!() };
        assert_eq!(q["query"]["stats"][0]["filters"][1], json!({ "id": pierce }));
    }

    fn gem(name: &str, level: &str, quality: u32, corrupted: bool) -> Item {
        let mut text = format!("Item Class: Skill Gems\nRarity: Gem\n{name}\n--------\nLevel: {level}\n");
        if quality > 0 {
            text += &format!("Quality: +{quality}% (augmented)\n");
        }
        if corrupted {
            text += "--------\nCorrupted\n";
        }
        crate::item::parse(&text).unwrap()
    }

    #[test]
    fn gem_filters_and_query() {
        let data = real_data();
        let item = gem("Arc", "20 (Max)", 20, true);
        assert!(searchable(&item));
        let filters = build_filters(&item, &data, Preset::Normal);
        let shown: Vec<(&str, bool, Option<f64>)> =
            filters.iter().map(|f| (f.label.as_str(), f.default_on, f.min)).collect();
        assert_eq!(shown, vec![("Gem level 20", true, Some(20.0)), ("Quality 20%", true, Some(20.0))]);
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, f.max))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::Yes,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Arc");
        // Minimums only: a better gem listed for less is competition.
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["gem_level"], json!({ "min": 20 }));
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["quality"], json!({ "min": 20 }));
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["corrupted"]["option"], "true");
        assert!(transfigured_type(&item, &data).is_none());

        // Below max level, 16% on a plain gem: level unticked, quality ticked.
        let filters = build_filters(&gem("Arc", "18", 16, false), &data, Preset::Normal);
        assert_eq!(filters.iter().map(|f| f.default_on).collect::<Vec<_>>(), vec![false, true]);
        assert!(filters.iter().all(|f| f.max.is_none()));
    }

    #[test]
    fn transfigured_gem() {
        let data = real_data();
        let (base, disc) = data.transfigured.iter().next().map(|(_, v)| v.clone()).expect("a transfigured Arc");
        let name = data.transfigured.iter().find(|(_, v)| v.1 == disc).unwrap().0.clone();
        let item = gem(&name, "20 (Max)", 16, false);
        assert_eq!(transfigured_type(&item, &data), Some(json!({ "option": base, "discriminator": disc })));
        // Harder to quality: 16% isn't ticked.
        let filters = build_filters(&item, &data, Preset::Normal);
        assert_eq!(filters.iter().map(|f| f.default_on).collect::<Vec<_>>(), vec![true, false]);
    }

    #[test]
    fn gem_listing_tag() {
        let item = json!({ "frameType": 4, "typeLine": "Arc", "properties": [
            { "name": "Level", "values": [["20 (Max)", 0]] },
            { "name": "Quality", "values": [["+23%", 1]] }
        ]});
        assert_eq!(gem_tag(&item).as_deref(), Some("Level 20 · 23%"));
        assert_eq!(item_preview(&item)[0], ("rarity", "gem".into(), None));
        assert!(gem_tag(&json!({ "frameType": 2 })).is_none());
    }

    fn stat_ids(q: &Value) -> Vec<Value> {
        q["query"]["stats"][0]["filters"].as_array().unwrap().clone()
    }

    #[test]
    fn cluster_jewel_enchants() {
        let data = real_data();
        let text = "Item Class: Jewels\nRarity: Normal\nLarge Cluster Jewel\n--------\nItem Level: 84\n--------\n\
                    Adds 8 Passive Skills (enchant)\n2 Added Passive Skills are Jewel Sockets (enchant)\n\
                    Added Small Passive Skills grant: 10% increased Spell Damage (enchant)\n";
        let item = crate::item::parse(text).unwrap();
        for preset in [Preset::Normal, Preset::Base] {
            let filters = build_filters(&item, &data, preset);
            let grant = filters.iter().find(|f| f.label.contains("Spell Damage")).expect("grant filter");
            assert!(grant.default_on && grant.min.is_none());
            let passives = filters.iter().find(|f| f.label == "Adds 8 Passive Skills").expect("passives");
            assert_eq!((passives.default_on, passives.min, passives.max), (true, Some(8.0), Some(8.0)));
            let sockets = filters.iter().find(|f| f.label.contains("Jewel Sockets")).expect("sockets");
            assert_eq!((sockets.default_on, sockets.min, sockets.max), (true, Some(2.0), None));

            let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, f.max))).collect();
            let opts = SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: true,
                corrupted: Corrupted::No,
                preset,
                status: Status::Instant,
            };
            let stats = stat_ids(&query(&item, &filters, &chosen, opts));
            assert!(
                stats.contains(&json!({ "id": "enchant.stat_3948993189", "value": { "option": 10 } })),
                "{stats:?}"
            );
            assert!(stats.contains(&json!({ "id": "enchant.stat_3086156145", "value": { "min": 8, "max": 8 } })));
        }

        // A two-line grant is one option.
        let text = "Item Class: Jewels\nRarity: Normal\nLarge Cluster Jewel\n--------\nItem Level: 84\n--------\n\
                    Adds 8 Passive Skills (enchant)\n\
                    Added Small Passive Skills grant: 12% increased Damage with Bows (enchant)\n\
                    Added Small Passive Skills grant: 12% increased Damage Over Time with Bow Skills (enchant)\n";
        let item = crate::item::parse(text).unwrap();
        let (options, picked) = option_filters(&item, &data);
        assert_eq!(picked.len(), 2);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].target, Target::StatOption("enchant.stat_3948993189".into(), 4));
    }

    #[test]
    fn anointment() {
        let data = real_data();
        let text = "Item Class: Amulets\nRarity: Rare\nDoom Locket\nOnyx Amulet\n--------\nItem Level: 84\n--------\n\
                    Allocates Charisma (enchant)\n--------\n+10 to all Attributes (implicit)\n--------\n+60 to maximum Life\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &data, Preset::Normal);
        let anoint = filters.iter().find(|f| f.label == "Allocates Charisma").expect("anointment filter");
        assert_eq!(anoint.target, Target::StatOption("enchant.stat_2954116742".into(), 6799));
        assert!(anoint.default_on);
        assert_eq!(anoint.section, Section::Enchant);
    }

    #[test]
    fn eldritch_implicits() {
        let data = real_data();
        let advanced = "Item Class: Gloves\nRarity: Rare\nDoom Grip\nSorcerer Gloves\n--------\nItem Level: 85\n--------\n\
                        { Searing Exarch Implicit Modifier (Grand) }\n+15(13-16)% to Fire Resistance (implicit)\n\
                        { Eater of Worlds Implicit Modifier (Lesser) }\n+5(5-8)% to Cold Resistance (implicit)\n--------\n\
                        { Prefix Modifier \"Virile\" (Tier: 3) — Life }\n+60(55-69) to maximum Life\n--------\n\
                        Searing Exarch Item\nEater of Worlds Item\n";
        let item = crate::item::parse(advanced).unwrap();
        assert_eq!((item.searing, item.tangled), (Some(Some(3)), Some(Some(1))));
        assert!(item.mods.iter().all(|m| !m.text.contains("Exarch Item")));
        let filters = build_filters(&item, &data, Preset::Normal);
        let searing = filters.iter().find(|f| f.label.starts_with("Searing Exarch")).unwrap();
        assert_eq!(searing.label, "Searing Exarch implicit: Grand+");
        assert!(matches!(&searing.target, Target::Stat(id) if id == "pseudo.pseudo_searing_implicit_tier"));
        assert_eq!((searing.min, searing.default_on), (Some(3.0), false));

        // Plain copy: only the flags.
        let plain = "Item Class: Gloves\nRarity: Rare\nDoom Grip\nSorcerer Gloves\n--------\nItem Level: 85\n--------\n\
                     +15% to Fire Resistance (implicit)\n--------\n+60 to maximum Life\n--------\nSearing Exarch Item\n";
        let item = crate::item::parse(plain).unwrap();
        let filters = build_filters(&item, &data, Preset::Base);
        let flag = filters.iter().find(|f| f.label == "Searing Exarch item").unwrap();
        assert_eq!(flag.target, Target::Flag("misc_filters", "searing_item"));
        assert!(flag.default_on);
    }

    /// Advanced copy heads eldritch implicits without an "(implicit)" after
    /// the mod: they're implicits all the same, each in its altar's section.
    /// A number without a range under a header is a fixed roll. The rows
    /// imply their altars, so the altars' own filters go.
    #[test]
    fn eldritch_implicit_rows() {
        let text = "Item Class: Gloves\nRarity: Rare\nDoom Hold\nSorcerer Gloves\n--------\nItem Level: 71\n--------\n\
                    { Searing Exarch Implicit Modifier (Lesser) \u{2014} Attack }\n\
                    Non-Vaal Strike Skills target 1 additional nearby Enemy\n\
                    { Eater of Worlds Implicit Modifier (Lesser) \u{2014} Life, Physical, Attack, Cold }\n\
                    0.2(0.2-0.3)% of Cold Damage Leeched as Life\n--------\n\
                    { Prefix Modifier \"Virile\" (Tier: 3) \u{2014} Life }\n+60(55-69) to maximum Life\n--------\n\
                    Searing Exarch Item\nEater of Worlds Item\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters
            .iter()
            .filter(|f| matches!(f.section, Section::Exarch | Section::Eater))
            .map(|f| (f.label.as_str(), f.min, f.section))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Non-Vaal Strike Skills target 1 additional nearby Enemy", Some(1.0), Section::Exarch),
                // 0.2 - 10% = 0.18, below the tier: raised to its floor
                ("0.2% of Cold Damage Leeched as Life", Some(0.2), Section::Eater),
            ]
        );
        let strike = filters.iter().find(|f| f.label.starts_with("Non-Vaal")).unwrap();
        assert_eq!(strike.target, Target::Stat("implicit.strike".into()));
    }

    /// A Vaal Orb's implicit has a header of its own in advanced copy: it's
    /// still an implicit to search, in its own section for the layer to
    /// color. Its reminder text isn't a mod.
    #[test]
    fn corruption_implicit_row() {
        let text = "Item Class: Gloves\nRarity: Rare\nBlood Fist\nSatin Gloves\n--------\nItem Level: 57\n--------\n\
                    { Corruption Implicit Modifier \u{2014} Caster, Curse }\n\
                    Curse Enemies with Conductivity on Hit \u{2014} Unscalable Value\n\
                    (Conductivity is a Hex which inflicts -17% to Lightning Resistance. It lasts 8 seconds)\n--------\n\
                    { Prefix Modifier \"Stout\" (Tier: 5) \u{2014} Life }\n+55(55-69) to maximum Life\n--------\nCorrupted\n";
        let item = crate::item::parse(text).unwrap();
        let implicits: Vec<_> = item.mods.iter().filter(|m| m.kind == ModKind::Implicit).collect();
        assert_eq!(implicits.len(), 1);
        assert!(implicits[0].corruption);
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let row = filters.iter().find(|f| f.label == "Curse Enemies with Conductivity on Hit").unwrap();
        assert_eq!(row.section, Section::Corruption);
        assert_eq!(row.target, Target::Stat("implicit.conductivity".into()));
    }

    #[test]
    fn search_options() {
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let enabled: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let opts = |base_only, corrupted| SearchOptions {
            six_link: false,
            unidentified: false,
            base_only,
            corrupted,
            preset: Preset::Normal,
            status: Status::Instant,
        };

        let q = query(&item, &filters, &enabled, opts(false, Corrupted::No));
        assert_eq!(q["query"]["filters"]["type_filters"]["filters"]["category"]["option"], "accessory.ring");
        assert!(q["query"]["type"].is_null());
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["corrupted"]["option"], "false");

        let q = query(&item, &filters, &enabled, opts(true, Corrupted::Any));
        assert_eq!(q["query"]["type"], "Sapphire Ring");
        assert!(q["query"]["filters"]["type_filters"]["filters"]["category"].is_null());
        assert!(q["query"]["filters"]["misc_filters"].is_null());

        let q = query(&item, &filters, &enabled, opts(false, Corrupted::Yes));
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["corrupted"]["option"], "true");
        // Instant buyout by default; the API calls it "securable".
        assert_eq!(q["query"]["status"]["option"], "securable");
        let q =
            query(&item, &filters, &enabled, SearchOptions { status: Status::Available, ..opts(false, Corrupted::No) });
        assert_eq!(q["query"]["status"]["option"], "available");
    }

    #[test]
    fn slider_ranges_and_max_rolls() {
        // Advanced copy: life rolled at the top of its tier, fire res not.
        let text = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                    { Prefix Modifier \"Virile\" (Tier: 3) — Life }\n+69(55-69) to maximum Life\n\
                    { Suffix Modifier \"of the Furnace\" (Tier: 4) }\n+30(30-35)% to Fire Resistance\n\
                    { Suffix Modifier \"of Plenty\" (Tier: 2) }\n12(8-14)% increased Rarity of Items found\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.min, f.slider, f.max_roll)).collect();
        assert_eq!(
            got,
            vec![
                // max roll: searched at exactly the roll
                ("+69 total maximum Life", Some(69.0), Some((55.0, 69.0)), true),
                // 30 - 10% = 27, below the tier: raised to its floor
                ("+30% total Elemental Resistance", Some(30.0), Some((30.0, 35.0)), false),
                ("12% increased Rarity of Items found", Some(10.0), Some((8.0, 14.0)), false),
            ]
        );
        // Totals sum several mods, so only listed mods carry a tier.
        let tiers: Vec<_> = filters.iter().map(|f| f.tier).collect();
        assert_eq!(tiers, vec![None, None, Some(2)]);
    }

    #[test]
    fn default_minimum_stays_in_tier() {
        let min_and_slider = |value, bounds| {
            let (min, slider, _, _) = defaults(value, Some(bounds), false);
            (min, slider)
        };
        // Mid-tier: roll - 10%, still inside the tier.
        assert_eq!(min_and_slider(80.0, (70.0, 89.0)), (Some(72.0), Some((70.0, 89.0))));
        // Bottom of the tier: the floor, not 9.
        assert_eq!(min_and_slider(11.0, (11.0, 13.0)), (Some(11.0), Some((11.0, 13.0))));
        // Below its own tier: the roll itself, so the item still matches.
        assert_eq!(min_and_slider(25.0, (30.0, 35.0)), (Some(25.0), Some((25.0, 35.0))));
    }

    #[test]
    fn sliders_only_for_real_ranges() {
        // Plain Ctrl+C: no roll ranges. The armour property keeps its slider;
        // the mod and the total don't (their wheel still steps).
        let plain = "Item Class: Gloves\nRarity: Rare\nDoom Hold\nSorcerer Gloves\n--------\nEnergy Shield: 65\n--------\n\
                     Item Level: 71\n--------\n+60 to maximum Life\n12% increased Rarity of Items found\n";
        let item = crate::item::parse(plain).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> =
            filters.iter().map(|f| (f.label.as_str(), f.slider.is_some(), f.draw_slider, f.section)).collect();
        assert_eq!(
            got,
            vec![
                ("Energy shield 78 (at 20% quality)", true, true, Section::Armour),
                ("+60 total maximum Life", true, false, Section::Total),
                ("12% increased Rarity of Items found", true, false, Section::Mod),
            ]
        );
        // Advanced copy: the tier ranges are real, so they're drawn.
        let advanced = "Item Class: Gloves\nRarity: Rare\nDoom Hold\nSorcerer Gloves\n--------\nItem Level: 71\n--------\n\
                        { Prefix Modifier \"Virile\" (Tier: 3) \u{2014} Life }\n+60(55-69) to maximum Life\n\
                        { Suffix Modifier \"of Plenty\" (Tier: 2) }\n12(8-14)% increased Rarity of Items found\n";
        let item = crate::item::parse(advanced).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let drawn: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.draw_slider)).collect();
        assert_eq!(drawn, vec![("+60 total maximum Life", true), ("12% increased Rarity of Items found", true)]);
    }

    #[test]
    fn unidentified_unique_by_name() {
        // An unidentified unique, named by the layer once its art matched
        // (the game's text has no name line).
        let text = "Item Class: Jewels\nRarity: Unique\nWatcher's Eye\nPrismatic Jewel\n--------\nItem Level: 86\n\
                    --------\nUnidentified\n";
        let item = crate::item::parse(text).unwrap();
        assert!(item.unidentified);
        assert!(searchable(&item));
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|_| None).collect();
        let opts = |unidentified| SearchOptions {
            six_link: false,
            unidentified,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts(false));
        assert_eq!(q["query"]["name"], "Watcher's Eye");
        assert_eq!(q["query"]["type"], "Prismatic Jewel");
        assert!(q["query"]["filters"]["misc_filters"]["filters"]["identified"].is_null());
        let q = query(&item, &filters, &chosen, opts(true));
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["identified"]["option"], "false");

        // Without a name there's nothing to search by.
        let unnamed = crate::item::parse(&text.replace("Watcher's Eye\n", "")).unwrap();
        assert!(!searchable(&unnamed));
    }

    #[test]
    fn unique_rolls() {
        let text = "Item Class: Body Armours\nRarity: Unique\nKaom's Heart\nGlorious Plate\n--------\nItem Level: 80\n--------\n\
                    { Unique Modifier — Life }\n+500 to maximum Life\n\
                    { Unique Modifier — Damage, Elemental, Fire }\n35(20-40)% increased Fire Damage\n\
                    { Unique Modifier — Elemental, Fire, Resistance }\n+40(30-40)% to Fire Resistance\n";
        let item = crate::item::parse(text).unwrap();
        assert!(searchable(&item));
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> =
            filters.iter().map(|f| (f.label.as_str(), f.min, f.slider, f.max_roll, f.default_on, f.section)).collect();
        assert_eq!(
            got,
            vec![
                // fixed life isn't listed; no totals; 35 - 10% of 20 = 33
                ("35% increased Fire Damage", Some(33.0), Some((20.0, 40.0)), false, false, Section::Mod),
                ("+40% to Fire Resistance", Some(40.0), Some((30.0, 40.0)), true, false, Section::Mod),
            ]
        );
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let q = query(
            &item,
            &filters,
            &chosen,
            SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: false,
                corrupted: Corrupted::No,
                preset: Preset::Normal,
                status: Status::Instant,
            },
        );
        assert_eq!(q["query"]["name"], "Kaom's Heart");
        assert_eq!(q["query"]["type"], "Glorious Plate");
        assert_eq!(q["query"]["filters"]["type_filters"]["filters"]["rarity"]["option"], "unique");
        assert!(q["query"]["filters"]["type_filters"]["filters"]["category"].is_null());
        assert_eq!(q["query"]["stats"][0]["filters"][0]["value"]["min"], 33.0);
    }

    #[test]
    fn unique_wrapped_mod() {
        // The Taming: the game wraps its second mod onto two lines.
        let text = "Item Class: Rings\nRarity: Unique\nThe Taming\nPrismatic Ring\n--------\nItem Level: 71\n--------\n\
                    { Unique Modifier — Elemental, Fire, Cold, Lightning, Ailment }\n\
                    10% chance to Freeze, Shock and Ignite\n\
                    (Freeze lowers Enemy Action Speed to zero, preventing them from acting.)\n\
                    { Unique Modifier — Damage, Elemental  — 20% Increased }\n\
                    33(30-40)% increased Elemental Damage with Hits and Ailments for\n\
                    each type of Elemental Ailment on Enemy\n\
                    { Unique Modifier — Elemental, Resistance }\n+25(20-30)% to all Elemental Resistances\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), &f.target)).collect();
        assert_eq!(
            got,
            vec![
                // The fixed 10% chance isn't listed, like Kaom's Heart's life.
                // 33 raised 20% by the header's catalyst: 39.6, shown as 39.
                (
                    "39% increased Elemental Damage with Hits and Ailments for each type of Elemental Ailment on Enemy",
                    &Target::Stat("explicit.ele_per_ailment".into())
                ),
                ("+25% to all Elemental Resistances", &Target::Stat("explicit.all_res".into())),
            ]
        );
        assert_eq!(filters[0].slider, Some((36.0, 48.0)));
    }

    #[test]
    fn catalysed_unique() {
        // A 34(30-40) roll under 20% catalyst quality shows, and is listed on
        // the trade site, as 40: searched as 40 within 36-48, not a perfect
        // roll. The resistances aren't raised, so +30 is.
        let text = "Item Class: Rings\nRarity: Unique\nThe Taming\nPrismatic Ring\n--------\n\
                    Quality (Elemental Damage Modifiers): +20% (augmented)\n--------\nItem Level: 70\n--------\n\
                    { Implicit Modifier — Elemental, Resistance }\n+10(8-10)% to all Elemental Resistances\n--------\n\
                    { Unique Modifier — Elemental, Fire, Cold, Lightning, Ailment  — 20% Increased }\n\
                    10% chance to Freeze, Shock and Ignite\n\
                    { Unique Modifier — Damage, Elemental  — 20% Increased }\n\
                    34(30-40)% increased Elemental Damage with Hits and Ailments for\n\
                    each type of Elemental Ailment on Enemy\n\
                    { Unique Modifier — Elemental, Resistance }\n+30(20-30)% to all Elemental Resistances\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters
            .iter()
            .filter(|f| f.section == Section::Mod)
            .map(|f| (f.label.as_str(), f.min, f.slider, f.draw_slider, f.max_roll, f.increased))
            .collect();
        assert_eq!(
            got,
            vec![
                // Fixed, but raised: copies differ by their quality, so it's
                // listed, at its value, with nothing to slide.
                ("12% chance to Freeze, Shock and Ignite", Some(12.0), None, false, false, Some(20)),
                (
                    "40% increased Elemental Damage with Hits and Ailments for each type of Elemental Ailment on Enemy",
                    Some(38.0),
                    Some((36.0, 48.0)),
                    true,
                    false,
                    Some(20)
                ),
                ("+30% to all Elemental Resistances", Some(30.0), Some((20.0, 30.0)), true, true, None),
            ]
        );
    }

    #[test]
    fn listing_preview() {
        let item = serde_json::json!({
            "frameType": 2, "name": "Doom Loop", "typeLine": "Sapphire Ring", "ilvl": 84, "identified": true,
            "implicitMods": ["+25% to Cold Resistance"],
            "explicitMods": [
                { "description": "+60 to maximum [Life]", "mods": [{ "tier": "P4" }, { "tier": "S2" }] },
                "+30 to [Strength|Strength]"
            ],
            "craftedMods": ["Enemies within 5 metres\nhave 10% reduced Life Regeneration"],
            "corrupted": true,
            "extended": { "pdps": 0.0 }
        });
        let got = item_preview(&item);
        let want: Vec<(&str, &str)> = vec![
            ("rarity", "rare"),
            ("name", "Doom Loop"),
            ("base", "Sapphire Ring"),
            ("ilvl", "84"),
            ("implicit", "+25% to Cold Resistance"),
            ("explicit", "+60 to maximum Life"),
            ("explicit", "+30 to Strength"),
            ("crafted", "Enemies within 5 metres"),
            ("crafted", "have 10% reduced Life Regeneration"),
            ("corrupted", "Corrupted"),
        ];
        assert_eq!(got.iter().map(|(k, t, _)| (*k, t.as_str())).collect::<Vec<_>>(), want);
        // Tiers: the best of the affixes behind a line; none for plain strings.
        let tiers: Vec<_> = got.iter().filter(|l| l.0 == "explicit").map(|l| l.2).collect();
        assert_eq!(tiers, vec![Some(2), None]);
        assert_eq!(affix_tier("R1"), None);
    }

    #[test]
    fn listing_preview_quality() {
        // A catalyst raised the resistances past their 20-30 range: the
        // quality line says why.
        let ring = serde_json::json!({
            "frameType": 3, "name": "The Taming", "typeLine": "Prismatic Ring", "ilvl": 70, "identified": true,
            "properties": [{ "name": "Quality (Resistance Modifiers)", "values": [["+20%", 1]], "displayMode": 0, "type": 6 }],
            "explicitMods": [{ "description": "+33% to all Elemental Resistances", "domain": "explicit",
                               "mods": [{ "magnitudes": [{ "min": "20", "max": "30" }] }] }]
        });
        let got: Vec<_> = item_preview(&ring).into_iter().map(|(k, t, _)| (k, t)).collect();
        assert_eq!(
            got[4..],
            [
                ("quality", "Quality (Resistance Modifiers): +20%".to_string()),
                ("explicit", "+33% to all Elemental Resistances".to_string()),
            ]
        );
        let armour = serde_json::json!({ "frameType": 2, "typeLine": "Glorious Plate",
            "properties": [{ "name": "[Quality]", "values": [["+20%", 1]] }, { "name": "[Armour]", "values": [["900", 1]] }] });
        assert!(item_preview(&armour).contains(&("quality", "Quality: +20%".to_string(), None)));
        let gem = serde_json::json!({ "frameType": 4, "typeLine": "Fireball",
            "properties": [{ "name": "Quality", "values": [["+20%", 1]] }] });
        assert!(!item_preview(&gem).iter().any(|l| l.0 == "quality"));
        let plain = serde_json::json!({ "frameType": 2, "typeLine": "Glorious Plate",
            "properties": [{ "name": "Quality", "values": [["+0%", 1]] }] });
        assert!(!item_preview(&plain).iter().any(|l| l.0 == "quality"));
    }

    #[test]
    fn socket_sprite_lookup() {
        let html = r#"<link href="https://web.poecdn.com/dist/css/chunk.A1.css" rel="stylesheet">
            <script src='https://web.poecdn.com/dist/js/x.js'></script>
            <link href='https://web.poecdn.com/dist/css/chunk.B2.css?v=3'>"#;
        assert_eq!(
            stylesheet_links(html),
            ["https://web.poecdn.com/dist/css/chunk.A1.css", "https://web.poecdn.com/dist/css/chunk.B2.css?v=3"]
        );
        let css = ".x .socket.socketStr{background:url(/protected/image/gen/socket.png?v=17&key=mf1B-Q) no-repeat}";
        assert_eq!(socket_sprite_path(css), Some("/protected/image/gen/socket.png?v=17&key=mf1B-Q"));
        assert_eq!(socket_sprite_path(".x{}"), None);
    }

    /// As the trade site sends them lately: fractured and crafted mods among
    /// explicitMods, told apart by domain.
    #[test]
    fn listing_preview_domains() {
        let item = serde_json::json!({
            "frameType": 2, "name": "Corruption Veil", "typeLine": "Ezomyte Burgonet", "ilvl": 85, "w": 2, "h": 2,
            "icon": "https://web.poecdn.com/gen/image/WzI1/5d663a6c33/HelmetStr9.png",
            "sockets": [
                { "group": 0, "sColour": "R" }, { "group": 0, "sColour": "G" }, { "group": 1, "sColour": "B" }
            ],
            "influences": { "shaper": true },
            "explicitMods": [
                { "description": "+50 to Intelligence", "flags": { "fractured": true }, "domain": "fractured",
                  "mods": [{ "tier": "S3" }] },
                { "description": "+116 to maximum Life", "domain": "explicit", "mods": [{ "tier": "P2" }] },
                { "description": "+25% to Fire Resistance", "flags": { "crafted": true }, "domain": "crafted",
                  "mods": [{ "tier": "R2" }] }
            ],
            "veiledMods": [{ "description": "Suffix02", "domain": "veiled", "mods": [{ "tier": "S2" }] }],
            "duplicated": true
        });
        let got = item_preview(&item);
        let want: Vec<(&str, &str, Option<u32>)> = vec![
            ("icon", "https://web.poecdn.com/gen/image/WzI1/5d663a6c33/HelmetStr9.png", None),
            ("cells", "2 2", None),
            ("rarity", "rare", None),
            ("name", "Corruption Veil", None),
            ("base", "Ezomyte Burgonet", None),
            ("ilvl", "85", None),
            ("sockets", "R-G B", None),
            ("influence", "shaper", None),
            ("fractured", "+50 to Intelligence", Some(3)),
            ("explicit", "+116 to maximum Life", Some(2)),
            ("crafted", "+25% to Fire Resistance", None),
            ("veiled", "Veiled Suffix", None),
            ("mirrored", "Mirrored", None),
        ];
        assert_eq!(got.iter().map(|(k, t, n)| (*k, t.as_str(), *n)).collect::<Vec<_>>(), want);
        // Images only from the CDN.
        let elsewhere = serde_json::json!({ "typeLine": "Ring", "icon": "https://example.com/x.png" });
        assert!(item_preview(&elsewhere).iter().all(|l| l.0 != "icon"));
    }

    #[test]
    fn fractured_and_base_presets() {
        let item = crate::item::parse(crate::item::tests::FRACTURED_CROWN).unwrap();
        assert!(base_preset_available(&item));

        // Normal: the fractured life counts in the total, and can also be
        // searched as fractured (unticked).
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(
            got,
            vec![
                ("Armour 420 (at 20% quality)", false, Section::Armour),
                ("Energy shield 84 (at 20% quality)", false, Section::Armour),
                ("+143 total maximum Life", true, Section::Total),
                ("+52 total to Intelligence", false, Section::Total),
                ("+143 to maximum Life", false, Section::Fractured),
            ]
        );
        assert_eq!(filters[4].target, Target::Stat("fractured.life".into()));

        // Base item: item level, the fractured flag, the fractured mod and a
        // top-tier magic mod, all ticked; the exact base.
        let filters = build_filters(&item, &test_data(), Preset::Base);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(
            got,
            vec![
                ("Item level 84", true, Section::Base),
                ("Fractured item", true, Section::Base),
                ("+143 to maximum Life", true, Section::Fractured),
                ("+52 to Intelligence", true, Section::Mod),
                ("Armour 420 (at 20% quality)", false, Section::Armour),
                ("Energy shield 84 (at 20% quality)", false, Section::Armour),
            ]
        );
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Base,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Paladin Crown");
        assert!(q["query"]["filters"]["type_filters"]["filters"]["category"].is_null());
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["fractured_item"]["option"], "true");
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"]["min"], 84.0);

        // Finished items aren't bases: crafted, corrupted, 20% quality.
        let crafted = crate::item::parse(&format!(
            "{}--------\n+10% to Fire Resistance (crafted)\n",
            crate::item::tests::FRACTURED_CROWN.replace("Fractured Item\n", "")
        ))
        .unwrap();
        assert!(crafted.mods.iter().any(|m| m.kind == ModKind::Crafted) && !base_preset_available(&crafted));
        let corrupted = crate::item::parse(&format!("{}Corrupted\n", crate::item::tests::FRACTURED_CROWN)).unwrap();
        assert!(!base_preset_available(&corrupted));
    }

    #[test]
    fn influence_toggles() {
        // Two influences: a toggle each, ticked for the base item, not for
        // the normal search; both searched when ticked.
        let text = crate::item::tests::FRACTURED_CROWN.replace("Fractured Item", "Shaper Item\nElder Item");
        let item = crate::item::parse(&text).unwrap();
        for (preset, on) in [(Preset::Base, true), (Preset::Normal, false)] {
            let filters = build_filters(&item, &test_data(), preset);
            let got: Vec<_> = filters
                .iter()
                .filter(|f| f.label.contains("influence"))
                .map(|f| (f.label.as_str(), f.default_on, f.section))
                .collect();
            assert_eq!(
                got,
                vec![
                    ("Has Shaper influence", on, Section::Influence("shaper")),
                    ("Has Elder influence", on, Section::Influence("elder"))
                ]
            );
            let chosen: Vec<Option<Bounds>> = filters
                .iter()
                .map(|f| (f.default_on || f.label.contains("influence")).then_some((f.min, None)))
                .collect();
            let opts = SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: false,
                corrupted: Corrupted::No,
                preset,
                status: Status::Instant,
            };
            let q = query(&item, &filters, &chosen, opts);
            let misc = &q["query"]["filters"]["misc_filters"]["filters"];
            assert_eq!(misc["shaper_item"]["option"], "true");
            assert_eq!(misc["elder_item"]["option"], "true");
        }
    }

    #[test]
    fn memory_strands() {
        // Like APT: a minimum at the item's strands, ticked at 60+, in every
        // preset; searched as misc "memory_level".
        let crown = crate::item::tests::FRACTURED_CROWN;
        let with = |n: u32| crown.replace("Item Level: 84", &format!("Memory Strands: {n}\n--------\nItem Level: 84"));
        assert_eq!(crate::item::parse(crown).unwrap().memory_strands, None);
        for (strands, on) in [(72, true), (35, false)] {
            let item = crate::item::parse(&with(strands)).unwrap();
            assert_eq!(item.memory_strands, Some(strands));
            assert!(!item.mods.iter().any(|m| m.text.starts_with("Memory")));
            for preset in [Preset::Base, Preset::Normal] {
                let filters = build_filters(&item, &test_data(), preset);
                let f = filters.iter().find(|f| f.label.starts_with("Memory Strands")).unwrap();
                assert_eq!(
                    (f.label.clone(), f.default_on, f.min),
                    (format!("Memory Strands {strands}"), on, Some(strands as f64))
                );
                let chosen: Vec<Option<Bounds>> =
                    filters.iter().map(|f| f.label.starts_with("Memory").then_some((f.min, None))).collect();
                let opts = SearchOptions {
                    six_link: false,
                    unidentified: false,
                    base_only: false,
                    corrupted: Corrupted::No,
                    preset,
                    status: Status::Instant,
                };
                let q = query(&item, &filters, &chosen, opts);
                assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["memory_level"]["min"], strands as f64);
            }
        }
    }

    #[test]
    fn facetors_lens() {
        // Searched by base, with at least its stored experience.
        let text = "Item Class: Stackable Currency\nRarity: Currency\nFacetor's Lens\n--------\n\
                    Stored Experience: 750,000,000\n--------\n\
                    Adds stored experience to a gem, up to its maximum level\n--------\n\
                    Right click this item then left click a gem to apply it.\n";
        let item = crate::item::parse(text).unwrap();
        assert!(searchable(&item));
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.min)).collect();
        assert_eq!(got, [("Stored experience 750,000,000", true, Some(750_000_000.0))]);
        let chosen = [Some((filters[0].min, None))];
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Facetor's Lens");
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["stored_experience"]["min"], 750_000_000.0);
        let listed = json!({ "properties": [{ "name": "Stored Experience", "values": [["1,500,000,000", 0]] }] });
        assert_eq!(listing_value(&listed, &filters[0].target), Some("1500000000".into()));
    }

    #[test]
    fn map_drop_bonuses() {
        let text = "Item Class: Maps\nRarity: Rare\nGrim Hollow\nBlighted Map (Tier 16)\n--------\nMap Tier: 16\n\
                    Item Rarity: +45% (augmented)\nMonster Pack Size: +28% (augmented)\n\
                    More Maps: +20% (augmented)\nMore Scarabs: +15% (augmented)\n--------\nItem Level: 83\n--------\n\
                    26% increased Monster Damage\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on)).collect();
        assert_eq!(
            got,
            [
                ("More scarabs +15%", true),
                ("More maps +20%", true),
                ("Pack size +28%", false),
                ("Rarity +45%", false),
                ("Blighted", true)
            ]
        );
        // The base names the tier, so it's searched by base, not "Any Map".
        assert!(tiered_map_base(&item));
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: true,
            corrupted: Corrupted::Any,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &[Some((Some(13.0), None)), None, None, None, Some((None, None))], opts);
        let q = &q["query"];
        assert_eq!(q["type"], "Blighted Map (Tier 16)");
        assert_eq!(q["filters"]["map_filters"]["filters"]["map_blighted"]["option"], "true");
        assert_eq!(q["stats"][0]["filters"][0]["id"], "pseudo.pseudo_map_more_scarab_drops");
    }

    #[test]
    fn linked_bases_and_defences() {
        // A white 6-link: priced as a base, 6-linked only (an option, not a
        // filter), item level unticked.
        let text = "Item Class: Body Armours\nRarity: Normal\nAstral Plate\n--------\n\
                    Quality: +20% (augmented)\nArmour: 853 (augmented)\n--------\n\
                    Sockets: R-R-G-G-B-B \n--------\nItem Level: 70\n--------\n\
                    +12% to all Elemental Resistances (implicit)\n";
        let item = crate::item::parse(text).unwrap();
        assert_eq!((item.links, item.sockets, item.armour), (6, 6, Some(853.0)));
        assert!(searchable(&item) && base_preset_available(&item));
        assert_eq!(default_preset(&item), Preset::Base);
        let filters = build_filters(&item, &test_data(), Preset::Base);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(got[0], ("Item level 70", false, Section::Base));
        assert!(got.iter().all(|f| !f.0.contains("links")), "{got:?}");
        assert_eq!(got.last(), Some(&("Armour 853", false, Section::Armour)));
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, None))).collect();
        let opts = SearchOptions {
            six_link: true,
            unidentified: false,
            base_only: true,
            corrupted: Corrupted::No,
            preset: Preset::Base,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Astral Plate");
        assert_eq!(q["query"]["filters"]["socket_filters"]["filters"]["links"]["min"], 6);
        let any = query(&item, &filters, &chosen, SearchOptions { six_link: false, ..opts });
        assert!(any["query"]["filters"]["socket_filters"].is_null());
        assert!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"].is_null());

        // A white 6-socket without links: not priced as a base by default.
        let unlinked = crate::item::parse(&text.replace("R-R-G-G-B-B", "R R G G B B")).unwrap();
        assert_eq!(default_preset(&unlinked), Preset::Normal);
        assert!(searchable(&unlinked) && base_preset_available(&unlinked));

        // A plain white base, no mods or links: searched as the base, item
        // level ticked when it's 82+.
        let plain = crate::item::parse(
            "Item Class: Body Armours\nRarity: Normal\nDesert Brigandine\n--------\n\
             Armour: 412\nEvasion Rating: 412\n--------\nRequirements:\nLevel: 60\nStr: 96\nDex: 96\n\
             --------\nSockets: R-R B \n--------\nItem Level: 84\n",
        )
        .unwrap();
        assert!(plain.mods.is_empty() && searchable(&plain));
        assert_eq!(default_preset(&plain), Preset::Base);
        let filters = build_filters(&plain, &test_data(), Preset::Base);
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, None))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: true,
            corrupted: Corrupted::No,
            preset: Preset::Base,
            status: Status::Instant,
        };
        let q = query(&plain, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Desert Brigandine");
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"]["min"], 84.0);

        // A white invitation has no mods, but has listings: searched by type.
        let invitation = crate::item::parse(
            "Item Class: Misc Map Items\nRarity: Normal\nScreaming Invitation\n--------\n\
             Modifiers to Item Quantity affect the amount of rewards dropped by the boss\n--------\n\
             Open portals to Absence of Symmetry and Harmony by using this item in a personal Map Device.\n",
        )
        .unwrap();
        assert!(invitation.mods.is_empty() && searchable(&invitation));
        assert_eq!(default_preset(&invitation), Preset::Normal);
        let filters = build_filters(&invitation, &test_data(), Preset::Normal);
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, None))).collect();
        let q =
            query(&invitation, &filters, &chosen, SearchOptions { base_only: false, preset: Preset::Normal, ..opts });
        assert_eq!(q["query"]["type"], "Screaming Invitation");

        // A rare 5-link at 10% quality with local increased armour and
        // evasion: defences scaled to 20% quality, no links filter.
        let text = "Item Class: Body Armours\nRarity: Rare\nGale Shell\nTriumphant Lamellar\n--------\n\
                    Quality: +10% (augmented)\nArmour: 1000 (augmented)\nEvasion Rating: 900 (augmented)\n\
                    --------\nSockets: R-G-G-B-B \n--------\nItem Level: 84\n--------\n\
                    90% increased Armour and Evasion\n+80 to maximum Life\n";
        let item = crate::item::parse(text).unwrap();
        assert_eq!(default_preset(&item), Preset::Normal);
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(
            got[..2],
            [
                ("Armour 1050 (at 20% quality)", false, Section::Armour),
                ("Evasion 945 (at 20% quality)", false, Section::Armour),
            ]
        );
        assert!(got.iter().all(|f| !f.0.contains("links")), "{got:?}");
        assert_eq!(filters[0].target, Target::Property("armour_filters", "ar"));
    }

    #[test]
    fn weapon_dps() {
        let text = "Item Class: Warstaves\nRarity: Rare\nBramble Mast\nEzomyte Staff\n--------\n\
                    Physical Damage: 100-200 (augmented)\nCritical Strike Chance: 6.50%\n\
                    Attacks per Second: 1.30 (augmented)\n--------\n\
                    Item Level: 68\n--------\n47% increased Physical Damage\n";
        let item = crate::item::parse(text).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        assert_eq!(filters[0].label, "Physical DPS 195");
        assert_eq!(filters[0].target, Target::Property("weapon_filters", "pdps"));
        assert!(filters[0].default_on);
        assert_eq!(filters[0].min, Some(175.0));
        assert_eq!(filters[1].label, "Attacks per second 1.30");
        assert_eq!(filters[1].min, Some(1.17));
        assert!(!filters[1].default_on);
        assert_eq!(filters[2].label, "Critical strike chance 6.50%");
        assert_eq!(filters[2].target, Target::Property("weapon_filters", "crit"));
        assert_eq!(filters[2].min, Some(5.85));
        assert!(!filters[2].default_on);
        // A whole-number crit chance still slides in hundredths.
        let text = text.replace("6.50%", "7.00%");
        let filters = build_filters(&crate::item::parse(&text).unwrap(), &test_data(), Preset::Normal);
        assert_eq!((filters[2].min, filters[2].step), (Some(6.3), 0.01));

        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, None))).collect();
        let q = query(
            &item,
            &filters,
            &chosen,
            SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: false,
                corrupted: Corrupted::Any,
                preset: Preset::Normal,
                status: Status::Instant,
            },
        );
        assert_eq!(q["query"]["filters"]["weapon_filters"]["filters"]["pdps"]["min"], 175.0);
        assert!(q["query"]["filters"]["weapon_filters"]["filters"]["aps"].is_null());

        // A chosen maximum goes along with the minimum; either may be left out.
        let mut chosen = chosen;
        chosen[0] = Some((Some(180.0), Some(220.0)));
        chosen[1] = Some((None, Some(1.5)));
        let q = query(
            &item,
            &filters,
            &chosen,
            SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: false,
                corrupted: Corrupted::Any,
                preset: Preset::Normal,
                status: Status::Instant,
            },
        );
        assert_eq!(
            q["query"]["filters"]["weapon_filters"]["filters"]["pdps"],
            serde_json::json!({ "min": 180, "max": 220 })
        );
        assert_eq!(q["query"]["filters"]["weapon_filters"]["filters"]["aps"], serde_json::json!({ "max": 1.5 }));
    }

    #[test]
    fn attributes() {
        assert_eq!(attribute_parts("+# to all Attributes"), Some([1.0, 1.0, 1.0]));
        assert_eq!(attribute_parts("+# to Strength and Dexterity"), Some([1.0, 1.0, 0.0]));
        assert_eq!(attribute_parts("+# to Level of Socketed Strength Gems"), None);
    }

    #[test]
    fn resists() {
        assert_eq!(resist_parts("+#% to Fire Resistance"), Some((1, false)));
        assert_eq!(resist_parts("+#% to Fire and Cold Resistances"), Some((2, false)));
        assert_eq!(resist_parts("+#% to all Elemental Resistances"), Some((3, false)));
        assert_eq!(resist_parts("+#% to Fire and Chaos Resistances"), Some((1, true)));
        assert_eq!(resist_parts("+# to maximum Life"), None);
    }

    #[test]
    fn local_stats_on_armour_are_only_defences() {
        let mut data = test_data();
        for text in ["+# to Accuracy Rating", "#% increased Attack Speed", "#% increased Energy Shield"] {
            data.stats.insert(("explicit".into(), text.into()), format!("global {text}"));
            data.stats.insert(("explicit".into(), format!("{text} (Local)")), format!("local {text}"));
        }
        let gloves = "Item Class: Gloves\nRarity: Rare\nDoom Hold\nSorcerer Gloves\n--------\n\
                      Energy Shield: 65 (augmented)\n--------\nItem Level: 71\n--------\n\
                      { Suffix Modifier \"of the Marksman\" (Tier: 3) — Attack }\n\
                      +304(251-350) to Accuracy Rating\n\
                      { Suffix Modifier \"of Mastery\" (Tier: 2) — Attack, Speed }\n\
                      11(11-13)% increased Attack Speed\n\
                      { Prefix Modifier \"Arcing\" (Tier: 4) — Defences, Energy Shield }\n\
                      40(39-50)% increased Energy Shield\n";
        let item = crate::item::parse(gloves).unwrap();
        assert_eq!(Local::of(&item), Local::Armour);
        let targets: Vec<_> = build_filters(&item, &data, Preset::Normal)
            .into_iter()
            .filter_map(|f| match f.target {
                Target::Stat(id) => Some(id),
                _ => None,
            })
            .collect();
        assert!(targets.contains(&"global +# to Accuracy Rating".to_string()), "{targets:?}");
        assert!(targets.contains(&"global #% increased Attack Speed".to_string()), "{targets:?}");
        assert!(targets.contains(&"local #% increased Energy Shield".to_string()), "{targets:?}");

        // On a weapon, accuracy and attack speed are its own.
        let t = |template| t_with(&data, Local::Weapon, template);
        assert_eq!(t("+# to Accuracy Rating").as_deref(), Some("local +# to Accuracy Rating"));
        assert_eq!(t("#% increased Attack Speed").as_deref(), Some("local #% increased Attack Speed"));
        // Reduced is searched as negative increased, still the local stat.
        assert_eq!(
            find_stat(&data, ModKind::Explicit, "#% reduced Attack Speed", &[], Local::Weapon),
            Some(Found {
                target: Target::Stat("local #% increased Attack Speed".to_string()),
                sign: -1.0,
                fixed: vec![]
            })
        );
        // A stat only listed as "(Local)", like the real "#% increased
        // Energy Shield", still matches off armour.
        data.stats.remove(&("explicit".into(), "#% increased Energy Shield".into()));
        assert_eq!(
            t_with(&data, Local::None, "#% increased Energy Shield"),
            Some("local #% increased Energy Shield".to_string())
        );

        // Like APT, a quiver's attack speed is global.
        let quiver = crate::item::parse(
            "Item Class: Quivers\nRarity: Rare\nDoom Quiver\nSerrated Arrow Quiver\n--------\nItem Level: 80\n\
             --------\n10% increased Attack Speed\n",
        )
        .unwrap();
        assert_eq!(Local::of(&quiver), Local::None);
        // Like APT, a fishing rod is a weapon.
        let rod = crate::item::parse(
            "Item Class: Fishing Rods\nRarity: Rare\nDoom Hook\nFishing Rod\n--------\nItem Level: 80\n\
             --------\n+300 to Accuracy Rating\n",
        )
        .unwrap();
        assert_eq!(Local::of(&rod), Local::Weapon);
        assert_eq!(
            t_with(&data, Local::of(&quiver), "#% increased Attack Speed"),
            Some("global #% increased Attack Speed".to_string())
        );
    }

    fn t_with(data: &TradeData, local: Local, template: &str) -> Option<String> {
        single_id(find_stat(data, ModKind::Explicit, template, &[], local))
    }

    /// The id `find_stat` found, when it's a single stat.
    fn single_id(found: Option<Found>) -> Option<String> {
        match found?.target {
            Target::Stat(id) => Some(id),
            _ => None,
        }
    }

    #[test]
    fn unlabelled_local_stats() {
        // Order as on the trade site: local first in one group, global first
        // in the other.
        let stats = serde_json::json!({"result": [
            {"id": "explicit", "entries": [
                {"id": "explicit.stat_2089652545", "text": "#% chance to Intimidate Enemies for 4 seconds on Hit"},
                {"id": "explicit.stat_78985352", "text": "#% chance to Intimidate Enemies for 4 seconds on Hit"},
            ]},
            {"id": "fractured", "entries": [
                {"id": "fractured.stat_3739863694", "text": "#% chance to Impale Enemies on Hit with Attacks"},
                {"id": "fractured.stat_725880290", "text": "#% chance to Impale Enemies on Hit with Attacks"},
            ]},
        ]});
        let data = TradeData::from_json(&stats, &Value::Null, &Value::Null);
        let find = |kind, template, local| single_id(find_stat(&data, kind, template, &[], local));
        let intimidate = "#% chance to Intimidate Enemies for 4 seconds on Hit";
        assert_eq!(find(ModKind::Explicit, intimidate, Local::Weapon).as_deref(), Some("explicit.stat_2089652545"));
        assert_eq!(find(ModKind::Explicit, intimidate, Local::Armour).as_deref(), Some("explicit.stat_78985352"));
        let impale = "#% chance to Impale Enemies on Hit with Attacks";
        assert_eq!(find(ModKind::Fractured, impale, Local::Weapon).as_deref(), Some("fractured.stat_725880290"));
        assert_eq!(find(ModKind::Fractured, impale, Local::None).as_deref(), Some("fractured.stat_3739863694"));
    }

    #[test]
    fn body_armour_without_life_mods() {
        let entry = |group: &str, stat: &str, text: &str| json!({ "id": format!("{group}.{stat}"), "text": text });
        let stats = json!({ "result": [
            { "id": "explicit", "entries": [
                entry("explicit", "stat_3299347043", "+# to maximum Life"),
                entry("explicit", "stat_regen", "Every 4 seconds, Regenerate #% of Life over one second"),
                entry("explicit", "stat_fire", "+#% to Fire Resistance"),
            ]},
            { "id": "crafted", "entries": [
                entry("crafted", "stat_3299347043", "+# to maximum Life"),
                entry("crafted", "stat_983749596", "#% increased maximum Life"),
            ]},
            { "id": "implicit", "entries": [entry("implicit", "stat_3299347043", "+# to maximum Life")] },
        ]});
        let data = TradeData::from_json(&stats, &Value::Null, &Value::Null);
        let armour = |mods: &str| {
            format!(
                "Item Class: Body Armours\nRarity: Rare\nCorpse Shell\nVaal Regalia\n--------\n\
                 Energy Shield: 400\n--------\nItem Level: 86\n--------\n{mods}"
            )
        };
        let fire = "{ Suffix Modifier \"of the Furnace\" (Tier: 4) — Elemental, Fire, Resistance }\n\
                    +30(30-35)% to Fire Resistance\n";
        let life = "{ Prefix Modifier \"Virile\" (Tier: 3) — Life }\n+60(55-69) to maximum Life\n";
        let no_life = |text: &str| {
            let item = crate::item::parse(text).unwrap();
            let filters = build_filters(&item, &data, Preset::Normal);
            let i = filters.iter().position(|f| f.label == "No life modifiers")?;
            Some((item, filters, i))
        };

        let (item, filters, i) = no_life(&armour(fire)).unwrap();
        assert!(!filters[i].default_on);
        let Target::NoneOf(ids) = &filters[i].target else { panic!("{:?}", filters[i].target) };
        assert_eq!(
            ids,
            &[
                "explicit.stat_3299347043",
                "implicit.stat_3299347043",
                "crafted.stat_3299347043",
                "crafted.stat_983749596",
                "explicit.stat_regen"
            ]
        );
        let chosen: Vec<Option<Bounds>> = (0..filters.len()).map(|j| (j == i).then_some((None, None))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::Any,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &chosen, opts);
        let not = &q["query"]["stats"][1];
        assert_eq!(not["type"], "not");
        assert_eq!(not["filters"][0], json!({ "id": "explicit.stat_3299347043" }));
        assert_eq!(not["filters"].as_array().unwrap().len(), ids.len());
        // Unticked, it adds nothing to the query.
        let q = query(&item, &filters, &vec![None; filters.len()], opts);
        assert_eq!(q["query"]["stats"].as_array().unwrap().len(), 1);

        // A mod tagged Life, any other item class, or no tags to go by: none.
        assert!(no_life(&armour(&format!("{life}{fire}"))).is_none());
        let helmet = armour(fire).replace("Body Armours", "Helmets");
        assert!(no_life(&helmet).is_none());
        assert!(no_life(&armour("+30% to Fire Resistance\n")).is_none());
    }

    // ---- against replies captured by fixtures/refresh.py ----

    fn poe2_data() -> TradeData {
        let stats: Value = serde_json::from_str(include_str!("../fixtures/poe2/trade-stats.json")).unwrap();
        let statics: Value = serde_json::from_str(include_str!("../fixtures/poe2/trade-static.json")).unwrap();
        TradeData::from_json(&stats, &statics, &Value::Null)
    }

    #[test]
    fn poe2_waystone_filters() {
        let path = format!("{}/fixtures/poe2/items/waystone-rare.txt", env!("CARGO_MANIFEST_DIR"));
        let item = crate::item::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
        let filters = build_filters(&item, &poe2_data(), Preset::Normal);
        let got: Vec<(&str, bool)> = filters.iter().map(|f| (f.label.as_str(), f.default_on)).collect();
        assert_eq!(
            got,
            [
                ("Tier 14", true),
                ("Waystone drop chance +75%", true),
                ("Pack size +34%", false),
                ("Rare monsters +28%", false),
                ("Revives 2", false),
            ]
        );
        assert_eq!(filters[1].target, Target::Property("map_filters", "map_bonus"));
    }

    /// PoE2's stats have no "+", and its rune and desecrated mods have
    /// groups of their own.
    #[test]
    fn poe2_stats() {
        let data = poe2_data();
        let find = |kind, text: &str, local| find_stat(&data, kind, &templatize(text).0, &[], local).map(|f| f.target);
        let stat = |id: &str| Some(Target::Stat(id.to_string()));
        assert_eq!(
            find(ModKind::Explicit, "+57 to Accuracy Rating", Local::Weapon),
            stat("explicit.stat_691932474"),
            "a weapon's accuracy is its local stat"
        );
        assert_eq!(find(ModKind::Explicit, "+57 to Accuracy Rating", Local::None), stat("explicit.stat_803737631"));
        assert_eq!(
            find(ModKind::Crafted, "+3.45% to Critical Hit Chance", Local::Weapon),
            stat("crafted.stat_518292764")
        );
        assert_eq!(
            find(ModKind::Rune, "54% increased Armour, Evasion and Energy Shield", Local::Armour),
            stat("rune.stat_3523867985")
        );
        assert_eq!(find(ModKind::Rune, "38% increased Physical Damage", Local::Weapon), stat("rune.stat_1509134228"));
        // A desecrated mod may be explicit on another item.
        assert_eq!(
            find(ModKind::Desecrated, "+256 to Armour", Local::Armour),
            Some(Target::AnyStat(vec!["desecrated.stat_3484657501".into(), "explicit.stat_3484657501".into()]))
        );
    }

    fn real_data() -> TradeData {
        let stats: Value = serde_json::from_str(include_str!("../fixtures/trade-stats.json")).unwrap();
        let statics: Value = serde_json::from_str(include_str!("../fixtures/trade-static.json")).unwrap();
        let items: Value = serde_json::from_str(include_str!("../fixtures/trade-items.json")).unwrap();
        TradeData::from_json(&stats, &statics, &items)
    }

    #[test]
    fn real_stat_list() {
        let data = real_data();
        // The pseudo totals are looked up by their exact text: if GGG
        // renames one, its filter silently disappears.
        for text in [
            "+# total maximum Life",
            "+#% total Elemental Resistance",
            "+#% total to Chaos Resistance",
            "+# total maximum Mana",
            "+# total to Strength",
            "+# total to Dexterity",
            "+# total to Intelligence",
        ] {
            assert!(data.stats.contains_key(&("pseudo".into(), text.into())), "no pseudo stat {text:?}");
        }
        // Stat ids are per group, with the group as their prefix.
        let life = |group: &str| data.stats[&(group.to_string(), "+# to maximum Life".to_string())].clone();
        for group in ["explicit", "implicit", "crafted", "fractured"] {
            assert!(life(group).starts_with(&format!("{group}.")), "{group}: {}", life(group));
        }
        assert_eq!(data.currencies.get("divine").map(String::as_str), Some("Divine Orb"));
        assert_eq!(data.currencies.get("alch").map(String::as_str), Some("Orb of Alchemy"));
        assert!(
            data.images.get("divine").is_some_and(|i| i.starts_with('/')),
            "icon path: {:?}",
            data.images.get("divine")
        );
    }

    #[test]
    fn same_text_stats() {
        let data = real_data();
        let target = |text: &str, mod_line: &str| {
            let item = crate::item::parse(text).unwrap();
            let filters = build_filters(&item, &data, Preset::Normal);
            let f = filters.iter().find(|f| f.label == mod_line).unwrap_or_else(|| {
                panic!("no {mod_line:?} in {:?}", filters.iter().map(|f| &f.label).collect::<Vec<_>>())
            });
            (item, f.target.clone(), filters)
        };
        let id = |s: &str| Target::Stat(s.to_string());

        // A tincture's "increased effect" is the tincture stat, not the
        // flask's one listed first with the same text.
        let tincture = "Item Class: Tinctures\nRarity: Magic\nEnriched Prismatic Tincture of Fury\n--------\n\
                        Inflicts Mana Burn every 0.7 seconds\nCooldown: 8 seconds\n--------\nItem Level: 82\n\
                        --------\n35% increased effect\n12% increased Cooldown Recovery Rate\n\
                        Melee Weapon Damage Penetrates 14% Elemental Resistances\n";
        let (item, effect, filters) = target(tincture, "35% increased effect");
        assert_eq!(Local::of(&item), Local::Tincture);
        assert_eq!(effect, id("explicit.stat_3529940209"));
        assert_eq!(target(tincture, "12% increased Cooldown Recovery Rate").1, id("explicit.stat_239144"));
        assert_eq!(
            target(tincture, "Melee Weapon Damage Penetrates 14% Elemental Resistances").1,
            id("explicit.stat_2499808413")
        );
        let all: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let options = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&item, &filters, &all, options);
        assert_eq!(q["query"]["stats"].as_array().unwrap().len(), 1, "{q}");
        assert!(
            q["query"]["stats"][0]["filters"].as_array().unwrap().iter().any(|f| f["id"] == "explicit.stat_3529940209")
        );

        // Anything else's is the other stat.
        let flask = "Item Class: Utility Flasks\nRarity: Magic\nAbecedarian's Granite Flask\n--------\n\
                     Lasts 4.80 Seconds\n--------\nItem Level: 82\n--------\n25% increased effect\n";
        assert_eq!(target(flask, "25% increased effect").1, id("explicit.stat_2448920197"));
        let jewel = "Item Class: Jewels\nRarity: Magic\nCrafty Cobalt Jewel\n--------\nItem Level: 82\n\
                     --------\n4% increased Cooldown Recovery Rate\n";
        assert_eq!(target(jewel, "4% increased Cooldown Recovery Rate").1, id("explicit.stat_1004011302"));

        // Other alike stats are searched as any of them.
        let ring = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                    +25% to Cold Resistance (implicit)\n--------\n\
                    { Suffix Modifier \"of Recharging\" (Tier: 1) — Defences, Energy Shield }\n\
                    Regenerate 10(8-12) Energy Shield per second\n";
        let (item, regen, filters) = target(ring, "Regenerate 10 Energy Shield per second");
        assert_eq!(regen, Target::AnyStat(vec!["explicit.stat_2561836520".into(), "explicit.stat_1330109706".into()]));
        let chosen: Vec<Option<Bounds>> =
            filters.iter().map(|f| (f.target == regen).then_some((Some(9.0), None))).collect();
        let q = query(&item, &filters, &chosen, options);
        assert_eq!(
            q["query"]["stats"],
            json!([
                { "type": "and", "filters": [] },
                { "type": "count", "value": { "min": 1 }, "filters": [
                    { "id": "explicit.stat_2561836520", "value": { "min": 9 } },
                    { "id": "explicit.stat_1330109706", "value": { "min": 9 } },
                ] },
            ])
        );
        // A listing shows whichever of them it has.
        let listed = json!({ "explicitMods": [
            { "hash": "stat.explicit.stat_1330109706", "description": "Regenerate 9 Energy Shield per second" },
        ] });
        assert_eq!(listing_value(&listed, &regen), Some("9".into()));
    }

    #[test]
    fn stats_with_fixed_numbers() {
        assert_eq!(
            number_slots("#% chance to gain Onslaught for 4 seconds on Kill"),
            ("#% chance to gain Onslaught for # seconds on Kill".into(), vec![None, Some(4.0)])
        );
        assert_eq!(number_slots("Trigger with a 0.25 second Cooldown").1, vec![Some(0.25)]);

        let data = real_data();
        let ring = |mod_line: &str| {
            let item = crate::item::parse(&format!(
                "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                 +25% to Cold Resistance (implicit)\n--------\n{mod_line}\n"
            ))
            .unwrap();
            build_filters(&item, &data, Preset::Normal).into_iter().find(|f| f.section == Section::Mod)
        };
        // The "4" is the stat's text, the 10 its value and roll.
        let onslaught = ring(
            "{ Suffix Modifier \"of Onslaught\" (Tier: 1) — Speed }\n\
             10(8-10)% chance to gain Onslaught for 4 seconds on Kill",
        )
        .unwrap();
        assert_eq!(onslaught.label, "10% chance to gain Onslaught for 4 seconds on Kill");
        assert_eq!(
            onslaught.target,
            Target::AnyStat(vec!["explicit.stat_3023957681".into(), "explicit.stat_665823128".into()])
        );
        assert_eq!((onslaught.roll, onslaught.slider), (Some(10.0), Some((8.0, 10.0))));
        // Another number there is another stat, which the trade site lacks.
        assert!(ring("10% chance to gain Onslaught for 6 seconds on Kill").is_none());
        // Only fixed numbers: searched by having it.
        let unholy = ring("Gain Unholy Might for 4 seconds on Critical Strike").unwrap();
        assert!(matches!(&unholy.target, Target::Stat(id) if id.starts_with("explicit.")), "{:?}", unholy.target);
        assert_eq!((unholy.roll, unholy.min), (None, None));
    }

    #[test]
    fn viscera_by_base_and_implicits() {
        let data = real_data();
        let limb = |base: &str, implicit: &str| {
            crate::item::parse(&format!(
                "Item Class: Viscera\n{base}\n--------\nUpper Limb\n--------\nItem Level: 29\n--------\n\
                 {{ Implicit Modifier }}\n{implicit}\n--------\n\
                 Take this to the Wickerman in a town or hideout to attach it.\n"
            ))
            .unwrap()
        };
        let lesser = limb("Lesser Brute's Arm", "+15(10-15) to Strength");
        assert!(searchable(&lesser));
        assert!(!base_preset_available(&lesser));
        let filters = build_filters(&lesser, &data, Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.min, f.max, f.default_on, f.section)).collect();
        // A top roll: exactly it, capped at the tier's best.
        assert_eq!(got, vec![("+15 to Strength", Some(15.0), Some(15.0), true, Section::Implicit)]);

        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, f.max))).collect();
        let options = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let q = query(&lesser, &filters, &chosen, options);
        // The trade site only knows the base without its tier.
        assert_eq!(q["query"]["type"], "Brute's Arm");
        assert!(q["query"]["filters"]["type_filters"]["filters"]["category"].is_null());
        let stat = &q["query"]["stats"][0]["filters"][0];
        assert_eq!(stat["id"], data.stats[&("implicit".to_string(), "+# to Strength".to_string())].as_str());
        assert_eq!(stat["value"]["min"], 15.0);
        assert_eq!(stat["value"]["max"], 15.0);

        let plain = limb("Brute's Arm", "+22(20-29) to Strength");
        let f = &build_filters(&plain, &data, Preset::Normal)[0];
        // 22 - 10% = 19, below the tier: raised to its floor.
        assert_eq!((f.min, f.max), (Some(20.0), Some(29.0)));
        assert_eq!(query(&plain, &[], &[], options)["query"]["type"], "Brute's Arm");

        // Plain Ctrl+C: no range, so no maximum; the note isn't a stat.
        let unranged = crate::item::parse(
            "Item Class: Viscera\nLesser Brute's Arm\n--------\nUpper Limb\n--------\nItem Level: 29\n--------\n\
             +15 to Strength\n--------\nTake this to the Wickerman in a town or hideout to attach it.\n",
        )
        .unwrap();
        let filters = build_filters(&unranged, &data, Preset::Normal);
        assert_eq!(filters.len(), 1);
        assert_eq!((filters[0].min, filters[0].max), (Some(13.0), None));
    }

    #[test]
    fn real_stat_ids_in_query() {
        let data = real_data();
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &data, Preset::Normal);
        // The same filters as with the hand-made stat list…
        let labels: Vec<&str> = filters.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "+75 total maximum Life",
                "+67% total Elemental Resistance",
                "+30 total to Strength",
                "+20 total to Dexterity",
                "+20 total to Intelligence",
                "12% increased Rarity of Items found",
            ]
        );
        // …each searched by an id that's in GGG's list.
        let ids: std::collections::HashSet<&String> = data.stats.values().collect();
        let all = vec![Some((None, None)); filters.len()];
        let q = query(
            &item,
            &filters,
            &all,
            SearchOptions {
                six_link: false,
                unidentified: false,
                base_only: false,
                corrupted: Corrupted::No,
                preset: Preset::Normal,
                status: Status::Instant,
            },
        );
        let searched: Vec<String> = q["query"]["stats"][0]["filters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(searched.len(), filters.len());
        for id in &searched {
            assert!(ids.contains(id), "{id} isn't a trade stat");
        }
        assert_eq!(searched[0], "pseudo.pseudo_total_life");
    }

    #[test]
    fn unticked_modifiers_go_in_disabled() {
        let data = real_data();
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &data, Preset::Normal);
        // Total life ticked, the rest not.
        let chosen: Vec<Option<Bounds>> =
            filters.iter().enumerate().map(|(i, f)| (i == 0).then_some((f.min, f.max))).collect();
        let opts = SearchOptions {
            six_link: false,
            unidentified: false,
            base_only: false,
            corrupted: Corrupted::No,
            preset: Preset::Normal,
            status: Status::Instant,
        };
        let mut q = query(&item, &filters, &chosen, opts);
        add_unticked(&mut q, &filters, &chosen);
        let stats = stat_ids(&q);
        assert_eq!(stats.len(), filters.len());
        assert_eq!(stats[0]["id"], "pseudo.pseudo_total_life");
        assert!(stats[0].get("disabled").is_none());
        for s in &stats[1..] {
            assert_eq!(s["disabled"], true, "{s}");
        }
        // With the filter's default minimum, ready to tick.
        let rarity = filters.iter().position(|f| f.label.contains("Rarity")).unwrap();
        assert_eq!(stats[rarity]["value"]["min"], json!(filters[rarity].min.unwrap() as i64));

        // The same query, in the trade site's address.
        let url = site_url(&q, "Covens");
        let (head, encoded) = url.split_once("?q=").unwrap();
        assert_eq!(head, "https://www.pathofexile.com/trade/search/Covens");
        assert!(!encoded.contains(['{', '"', ' ', '&', '#']));
        let decoded = String::from_utf8(
            encoded
                .split('%')
                .enumerate()
                .flat_map(|(i, part)| {
                    if i == 0 {
                        part.bytes().collect::<Vec<_>>()
                    } else {
                        let mut v = vec![u8::from_str_radix(&part[..2], 16).unwrap()];
                        v.extend(part[2..].bytes());
                        v
                    }
                })
                .collect(),
        )
        .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&decoded).unwrap(), q);

        // All ticked: nothing added.
        let all: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, f.max))).collect();
        let before = query(&item, &filters, &all, opts);
        let mut after = before.clone();
        add_unticked(&mut after, &filters, &all);
        assert_eq!(before, after);
    }

    const SEARCH: &str = include_str!("../fixtures/trade-search.json");
    const FETCH: &str = include_str!("../fixtures/trade-fetch.json");

    /// Half an hour after the newest listing in FETCH.
    fn fetch_now() -> i64 {
        let v: Value = serde_json::from_str(FETCH).unwrap();
        let newest =
            v["result"].as_array().unwrap().iter().filter_map(|e| parse_utc(e["listing"]["indexed"].as_str()?)).max();
        newest.unwrap() + 30 * 60
    }

    #[test]
    fn search_reply() {
        let (id, total, hashes) = parse_search(SEARCH).unwrap();
        assert!(!id.is_empty());
        assert_eq!(hashes.len(), LISTINGS);
        assert!(total >= hashes.len() as u64);
        assert!(hashes.iter().all(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())), "{hashes:?}");
        assert!(parse_search(r#"{"error":{"code":2,"message":"Invalid query"}}"#).is_err());
        assert!(parse_search("<html>Cloudflare</html>").is_err());
    }

    #[test]
    fn fetch_reply() {
        let prices = crate::ninja::tests::fixture_db();
        let singles = parse_fetch(FETCH, &real_data(), &prices, fetch_now(), &[]).unwrap();
        assert_eq!(singles.len(), LISTINGS, "every captured listing is priced");
        for (amount, id, name, chaos, minutes, preview, _) in &singles {
            // Currency names from the static list, chaos values from poe.ninja.
            match id.as_str() {
                "chaos" => assert_eq!((name.as_str(), *chaos), ("Chaos Orb", Some(*amount))),
                "alch" => assert_eq!(
                    (name.as_str(), *chaos),
                    ("Orb of Alchemy", Some(amount * prices.exchange["Orb of Alchemy"]))
                ),
                // Cheap rings sell for other small currency too.
                other => assert_eq!(
                    *chaos,
                    Some(amount * prices.exchange.get(name).copied().unwrap_or_else(|| panic!("{other}: {name}"))),
                    "{other}"
                ),
            }
            assert!(minutes.is_some_and(|m| m >= 30), "age {minutes:?}");
            // The listing's item, from today's mod format (objects with tiers).
            assert_eq!(preview[0], ("rarity", "rare".into(), None));
            assert!(preview.contains(&("base", "Sapphire Ring".into(), None)), "{preview:?}");
            assert!(preview.iter().any(|l| l.0 == "explicit" && l.2.is_some()), "no tiered explicit in {preview:?}");
            assert!(preview.iter().all(|l| !l.1.contains('[')), "markup left in {preview:?}");
        }
    }

    #[test]
    fn fetched_listings_grouped() {
        let prices = crate::ninja::tests::fixture_db();
        let singles = parse_fetch(FETCH, &real_data(), &prices, fetch_now(), &[]).unwrap();
        let (id, total, _) = parse_search(SEARCH).unwrap();
        let prices_in_order: Vec<(f64, String)> = singles.iter().map(|s| (s.0, s.1.clone())).collect();
        let result = trade_result(singles, total, &id, "Covens");
        assert_eq!(result.total, total);
        assert_eq!(result.url, format!("https://www.pathofexile.com/trade/search/Covens/{id}"));
        // Consecutive listings at one price make one row; counts add up.
        assert_eq!(result.listings.iter().map(|l| l.count).sum::<usize>(), prices_in_order.len());
        let mut rows = prices_in_order.clone();
        rows.dedup();
        let got: Vec<(f64, String)> = result.listings.iter().map(|l| (l.amount, l.currency_id.clone())).collect();
        assert_eq!(got, rows);
        for l in &result.listings {
            assert!(!l.age.is_empty(), "row without an age");
            assert!(l.age.split('–').all(|a| a.ends_with(['m', 'h', 'd'])), "age {:?}", l.age);
        }
        let summary = result.summary.expect("an estimate");
        let currencies: std::collections::HashSet<_> = prices_in_order.iter().map(|p| &p.1).collect();
        if currencies.len() > 1 {
            assert!(summary.range.ends_with('c'), "mixed currencies are compared in chaos: {}", summary.range);
        }
    }

    #[test]
    fn fetched_values() {
        let prices = crate::ninja::tests::fixture_db();
        let total_life = Target::Stat("pseudo.pseudo_total_life".into());
        let life = Target::Stat("explicit.stat_3299347043".into());
        let ilvl = Target::Property("misc_filters", "ilvl");
        let flag = Target::Flag("misc_filters", "fractured_item");
        let columns = [(0, &total_life), (1, &life), (4, &ilvl), (7, &flag)];
        let singles = parse_fetch(FETCH, &real_data(), &prices, fetch_now(), &columns).unwrap();
        let fetched: Value = serde_json::from_str(FETCH).unwrap();
        for ((.., values), entry) in singles.iter().zip(fetched["result"].as_array().unwrap()) {
            let item = &entry["item"];
            let get = |i: usize| values.iter().find(|v| v.0 == i).map(|v| v.1.clone());
            // The search asked for 60 total life and the fetch for the total.
            let total: f64 = get(0).expect("a total for every listing").parse().unwrap();
            assert!(total >= 60.0, "{total}");
            // Explicit life, read from the item's own mod text.
            let explicit = item["explicitMods"].as_array().unwrap().iter().find_map(|m| {
                let text = m["description"].as_str()?;
                let n = text.strip_prefix('+')?.strip_suffix(" to maximum Life")?;
                (m["domain"] == "explicit").then(|| n.to_string())
            });
            assert_eq!(get(1), explicit, "{item}");
            assert_eq!(get(4), Some(item["ilvl"].as_u64().unwrap().to_string()));
            assert_eq!(get(7), None, "flags have no value");
        }
    }

    #[test]
    fn listing_values() {
        // The older format: plain strings, their stats by place in extended.hashes.
        let item = json!({
            "explicitMods": ["Adds 5 to 10 Fire Damage to Attacks", "+38% to Cold Resistance"],
            "extended": { "hashes": { "explicit": [["explicit.stat_1", [1]], ["explicit.stat_2", [0]]] } },
            "properties": [{ "name": "Critical Strike Chance", "values": [["6.50%", 1]] }],
        });
        let value = |t: Target| listing_value(&item, &t);
        assert_eq!(value(Target::Stat("explicit.stat_1".into())), Some("7.5".into()));
        assert_eq!(value(Target::Stat("explicit.stat_2".into())), Some("38%".into()));
        assert_eq!(value(Target::Stat("explicit.stat_3".into())), None);
        assert_eq!(value(Target::Property("weapon_filters", "crit")), Some("6.5%".into()));
        // An implicit and a crafted mod of one stat add up.
        let item = json!({
            "explicitMods": [
                { "description": "+20 to maximum Life", "hash": "stat.explicit.stat_9" },
                { "description": "+25 to maximum Life", "hash": "stat.explicit.stat_9", "domain": "crafted" },
            ],
        });
        assert_eq!(listing_value(&item, &Target::Stat("explicit.stat_9".into())), Some("45".into()));
    }

    #[test]
    fn rows_with_different_values_stay_apart() {
        let single = |amount: f64, life: &str| -> Single {
            let values = vec![(0, life.to_string())];
            (amount, "chaos".into(), "Chaos Orb".into(), Some(amount), Some(60), Preview::default(), values)
        };
        let singles = vec![single(1.0, "158"), single(1.0, "158"), single(1.0, "171"), single(2.0, "171")];
        let rows = trade_result(singles, 4, "id", "Covens").listings;
        let got: Vec<(f64, usize, &str)> = rows.iter().map(|l| (l.amount, l.count, l.values[0].1.as_str())).collect();
        assert_eq!(got, vec![(1.0, 2, "158"), (1.0, 1, "171"), (2.0, 1, "171")]);
    }

    #[test]
    fn column_names() {
        for (label, name) in [
            ("+172 total maximum Life", "Life"),
            ("+86% total Elemental Resistance", "Ele res"),
            ("+18 to maximum Energy Shield", "ES"),
            ("+30% to Cold Resistance", "Cold res"),
            ("Physical DPS 312", "pDPS"),
            ("Energy shield 210 (at 20% quality)", "ES"),
            ("Evasion 340", "Evasion"),
            ("Item level 85", "ilvl"),
            ("Elemental DPS 120", "eDPS"),
            ("+42 total to Strength", "Str"),
            ("12% increased Movement Speed", "Move speed"),
            ("Adds 5 to 10 Physical Damage to Attacks", "Phys dmg Attacks"),
        ] {
            assert_eq!(column_name(label), name, "{label}");
        }
    }

    fn reply(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: String::new(),
        }
    }

    #[test]
    fn rate_limits() {
        // The search endpoint's headers, as GGG sent them for the fixture.
        let fresh = reply(
            200,
            &[
                ("x-rate-limit-rules", "Ip"),
                ("x-rate-limit-ip", "5:10:60,15:60:300,30:300:1800,600:21600:3600"),
                ("x-rate-limit-ip-state", "1:10:0,1:60:0,1:300:0,1:21600:0"),
            ],
        );
        // Limits unknown: a search (or fetch) every 5 s, like APT; reference
        // data isn't limited.
        let mut limiter = RateLimiter::default();
        assert!(limiter.check("search").is_ok(), "limits unknown: the first is allowed");
        limiter.record("search", &reply(200, &[]));
        assert!(limiter.check("search").is_err(), "limits unknown: one every 5 s");
        assert!(limiter.check("fetch").is_ok() && limiter.check("data").is_ok());

        let mut limiter = RateLimiter::default();
        // 5 per 10s, one kept spare: the fifth request is refused.
        for n in 1..=4 {
            limiter.record("search", &fresh);
            assert_eq!(limiter.check("search").is_ok(), n < 4, "after {n} requests");
        }
        assert!(limiter.check("fetch").is_ok(), "limits are per endpoint");

        // A penalty in the state header blocks every endpoint for that long.
        let mut limiter = RateLimiter::default();
        limiter.record(
            "fetch",
            &reply(
                200,
                &[("x-rate-limit-rules", "Ip"), ("x-rate-limit-ip", "12:4:10"), ("x-rate-limit-ip-state", "13:4:10")],
            ),
        );
        let err = limiter.check("search").unwrap_err();
        assert!(err.contains("try again in 10s") || err.contains("try again in 11s"), "{err}");

        // 429 without limit headers: Retry-After.
        let mut limiter = RateLimiter::default();
        limiter.record("search", &reply(429, &[("retry-after", "120")]));
        let err = limiter.check("search").unwrap_err();
        assert!(err.contains("in 120s") || err.contains("in 121s"), "{err}");
    }
}
