//! Official trade site search for rare/magic items, sent through the
//! headless browser (see cdp.rs for why).

use crate::cdp::{self, Browser, HttpResponse};
use crate::item::{Item, ModKind, ModLine};
use crate::ninja::{self, PriceDb};
use crate::settings::min_fraction;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Stat/currency reference data is refetched after this long.
const DATA_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The browser is closed after this long without a search.
pub const BROWSER_IDLE: Duration = Duration::from_secs(10 * 60);
/// Listings fetched per search (one fetch request allows up to 10).
const LISTINGS: usize = 10;

/// What a filter searches: a stat (by trade id) or an item property such
/// as weapon DPS (by its group and key in the query's filters).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Stat(String),
    /// A stat whose value is one of a list (a cluster jewel's grant, an
    /// anointed notable): its id and the option's number. The stat list
    /// names each option in full ("Allocates Charisma" is
    /// "enchant.stat_2954116742|6799").
    StatOption(String, u64),
    Property(&'static str, &'static str),
    /// A yes/no item filter, e.g. misc_filters.fractured_item.
    Flag(&'static str, &'static str),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Section {
    Weapon,
    Total,
    Mod,
    Implicit,
    Enchant,
    /// A fractured mod, searched as fractured: always shown.
    Fractured,
    /// Base item properties: item level, fractured/synthesised.
    Base,
    /// An influence flag, by its trade key ("shaper"): the layer colors it.
    Influence(&'static str),
    /// Armour, evasion, energy shield, ward, block.
    Armour,
    /// Links and sockets.
    Sockets,
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
            Section::Enchant => "enchant",
            Section::Fractured => "fractured",
            Section::Base => "base",
            Section::Influence(influence) => influence,
            Section::Armour => "armour",
            Section::Sockets => "sockets",
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
    /// Range for the minimum's slider: the tier's roll range (summed for
    /// totals) when advanced copy gave it, else around the value.
    pub slider: Option<(f64, f64)>,
    pub step: f64,
    /// The roll is at the top of its tier's range.
    pub max_roll: bool,
    /// Ticked by default (like Awakened PoE Trade: total life, the
    /// resistance totals and the main weapon DPS; everything else unticked).
    pub default_on: bool,
    pub section: Section,
    /// The mod's affix tier (1 is best), for listed mods from advanced copy.
    pub tier: Option<u32>,
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
            label,
            target,
            min,
            slider,
            step,
            max_roll,
            default_on: on,
            section,
            tier: None,
            roll: Some(value),
            max: None,
        }
    }

    /// A unique's rolled mod, unticked. Like APT, the minimum is the roll
    /// minus 10% of the mod's range (a perfect roll: exactly the roll).
    fn unique(label: String, target: Target, value: f64, (lo, hi): (f64, f64), section: Section) -> Filter {
        let mut f = Filter::new(label, target, value, Some((lo, hi)), false, section);
        if let (Some(_), false) = (f.min, f.max_roll) {
            let min =
                ((round_down(value - (hi - lo) * (1.0 - min_fraction()), f.step)) / f.step).round() / (1.0 / f.step);
            f.min = Some(min);
            f.slider = Some((lo.min(min), hi));
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
    /// (stat group, text template) -> trade stat id
    stats: HashMap<(String, String), String>,
    /// currency id ("divine") -> name ("Divine Orb")
    currencies: HashMap<String, String>,
    /// currency id -> icon path on web.poecdn.com
    images: HashMap<String, String>,
    /// Transfigured gems: name ("Arc of Oscillating") -> the base gem
    /// ("Arc") and the trade site's discriminator ("alt_y").
    transfigured: HashMap<String, (String, String)>,
}

impl TradeData {
    /// From /api/trade/data/stats, /api/trade/data/static and
    /// /api/trade/data/items.
    fn from_json(stats: &Value, statics: &Value, items: &Value) -> TradeData {
        let mut data = TradeData {
            stats: HashMap::new(),
            currencies: HashMap::new(),
            images: HashMap::new(),
            transfigured: HashMap::new(),
        };
        for group in items["result"].as_array().into_iter().flatten().filter(|g| g["id"] == "gem") {
            for e in group["entries"].as_array().into_iter().flatten() {
                if let (Some(base), Some(text), Some(disc)) =
                    (e["type"].as_str(), e["text"].as_str(), e["disc"].as_str())
                {
                    data.transfigured.insert(text.to_string(), (base.to_string(), disc.to_string()));
                }
            }
        }
        for group in stats["result"].as_array().into_iter().flatten() {
            let group_id = group["id"].as_str().unwrap_or("");
            for e in group["entries"].as_array().into_iter().flatten() {
                if let (Some(id), Some(text)) = (e["id"].as_str(), e["text"].as_str()) {
                    data.stats.entry((group_id.to_string(), text.to_string())).or_insert_with(|| id.to_string());
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
}

pub struct Trade {
    browser: Option<Browser>,
    data: Option<(TradeData, Instant)>,
    limiter: RateLimiter,
    last_used: Instant,
}

/// Items worth a trade search: anything non-unique with mods, and white
/// gear (searched as a base).
pub fn searchable(item: &Item) -> bool {
    match item.rarity.as_str() {
        "Rare" | "Magic" => !item.mods.is_empty() || item.links >= 5,
        "Normal" => !item.mods.is_empty() || base_preset_available(item),
        // Identified uniques: searched by name, with their rolls.
        "Unique" => item.name.is_some() && !item.mods.is_empty(),
        // Gems by name, level and quality.
        "Gem" => true,
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

impl Trade {
    pub fn new() -> Trade {
        Trade { browser: None, data: None, limiter: RateLimiter::default(), last_used: Instant::now() }
    }

    pub fn close_if_idle(&mut self) {
        if self.browser.is_some() && self.last_used.elapsed() > BROWSER_IDLE {
            eprintln!("trade: closing idle browser");
            self.browser = None;
        }
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
        // Like APT: an uncorrupted item is compared with uncorrupted ones.
        // A base or a unique is worth something else corrupted (no more
        // crafting, a different implicit), so it's compared like for like.
        let like_for_like = preset == Preset::Base || item.rarity == "Unique" || item.rarity == "Gem";
        let corrupted = match (item.corrupted, like_for_like) {
            (false, _) => Corrupted::No,
            (true, true) => Corrupted::Yes,
            (true, false) => Corrupted::Any,
        };
        // A map's base names its tier ("Map (Tier 16)"): search that base.
        let base_only = preset == Preset::Base || tiered_map_base(item);
        Ok(Prepared {
            filters,
            category_label,
            base_preset: base_preset_available(item),
            defaults: SearchOptions { base_only, corrupted, preset, status: crate::settings::default_status() },
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
        let data = &self.data.as_ref().unwrap().0;
        let filters = build_filters(item, data, options.preset);
        let chosen: Vec<Option<Bounds>> = filters
            .iter()
            .enumerate()
            .map(|(i, f)| enabled.iter().find(|c| c.index == i).map(|c| (c.min.or(f.min), c.max)))
            .collect();
        let mut body = query(item, &filters, &chosen, options);
        if let Some(t) = transfigured_type(item, data) {
            body["query"]["type"] = t;
        }
        let body = body.to_string();
        let league_path = ninja::encode(league);

        let resp = self.request("search", "POST", &format!("/api/trade/search/{league_path}"), Some(&body))?;
        let (id, total, hashes) = parse_search(&resp.body)?;

        // One entry per listing first.
        let mut singles: Vec<Single> = Vec::new();
        if !hashes.is_empty() {
            let path = format!("/api/trade/fetch/{}?query={id}", hashes.join(","));
            let resp = self.request("fetch", "GET", &path, None)?;
            let data = &self.data.as_ref().unwrap().0;
            singles = parse_fetch(&resp.body, data, prices, unix_now())?;
        }
        Ok(trade_result(singles, total, &id, &league_path))
    }

    /// A currency's icon (PNG), from web.poecdn.com (a plain CDN, no browser
    /// needed), cached on disk.
    pub fn icon(&mut self, currency_id: &str) -> Result<Vec<u8>, String> {
        if currency_id.is_empty() || !currency_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err("bad currency id".into());
        }
        let path = cache_dir().join("icons").join(format!("{currency_id}.png"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(bytes);
        }
        self.ensure_data()?;
        let data = &self.data.as_ref().unwrap().0;
        let image = data.images.get(currency_id).ok_or("no icon for this currency")?;
        let url = format!("https://web.poecdn.com{image}");
        let resp = ureq::get(&url)
            .set("User-Agent", ninja::USER_AGENT)
            .timeout(Duration::from_secs(20))
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        let mut bytes = Vec::new();
        resp.into_reader().take(1 << 20).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, &bytes);
        Ok(bytes)
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
        let resp = self.request("data", "GET", &format!("/api/trade/data/{name}"), None)?;
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
/// `now`: seconds since the epoch, for the listings' ages.
fn parse_fetch(body: &str, data: &TradeData, prices: &PriceDb, now: i64) -> Result<Vec<Single>, String> {
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
        let chaos = if currency == "chaos" { Some(amount) } else { prices.exchange.get(&name).map(|c| c * amount) };
        let minutes = listing["indexed"].as_str().and_then(parse_utc).map(|then| (now - then).max(0) / 60);
        singles.push((amount, currency.to_string(), name, chaos, minutes, item_preview(&entry["item"])));
    }
    Ok(singles)
}

fn gem_line(preview: &Preview) -> Option<&str> {
    preview.iter().find(|l| l.0 == "gem").map(|l| l.1.as_str())
}

/// Listings at the same price grouped, with the estimate.
fn trade_result(singles: Vec<Single>, total: u64, id: &str, league_path: &str) -> TradeResult {
    let summary = estimate_index(&singles).map(|(i, capped)| {
        let (amount, id, name, chaos, _, _) = &singles[i];
        let same_currency = singles.iter().all(|s| s.1 == *id);
        let (first, last) = (&singles[0], &singles[singles.len() - 1]);
        let range = if same_currency {
            format!("{}–{} {}", trim_number(first.0), trim_number(last.0), name)
        } else {
            let c = |s: &Single| s.3.map(trim_number).unwrap_or("?".into());
            format!("{}–{}c", c(first), c(last))
        };
        Summary { capped, amount: *amount, currency: name.clone(), currency_id: id.clone(), chaos: *chaos, range }
    });

    let mut listings: Vec<Listing> = Vec::new();
    let mut ages: Vec<Vec<i64>> = Vec::new();
    for (amount, id, name, chaos, minutes, preview) in singles {
        match listings.last_mut() {
            // Gems at one price but different levels or quality stay apart.
            Some(last)
                if last.amount == amount && last.currency_id == id && gem_line(&last.preview) == gem_line(&preview) =>
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

    TradeResult {
        listings,
        summary,
        total,
        url: format!("https://www.pathofexile.com/trade/search/{league_path}/{id}"),
    }
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    base.join("poe-waytrade")
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
        "Maps" => ("map", "Map"),
        c if c.ends_with("Flasks") => ("flask", "Flask"),
        _ => return None,
    })
}

fn find_stat(data: &TradeData, kind: ModKind, template: &str, local: bool) -> Option<(String, f64)> {
    let group = kind.trade_group().to_string();
    let mut candidates = Vec::new();
    // Weapons/armour list their own-item mods as separate "(Local)" stats.
    if local {
        candidates.push((format!("{template} (Local)"), 1.0));
    }
    candidates.push((template.to_string(), 1.0));
    // "10% reduced X" is searched as "#% increased X" with a negative value.
    for (from, to) in [("reduced", "increased"), ("less", "more")] {
        if template.contains(from) {
            candidates.push((template.replace(from, to), -1.0));
        }
    }
    if let Some(rest) = template.strip_prefix("-#") {
        candidates.push((format!("+#{rest}"), -1.0));
    }
    // Some stats are only searchable as pseudo stats (item rarity, for one).
    candidates.into_iter().find_map(|(t, sign)| {
        data.stats
            .get(&(group.clone(), t.clone()))
            .or_else(|| data.stats.get(&("pseudo".to_string(), t)))
            .map(|id| (id.clone(), sign))
    })
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
/// start ticked. Other mods are listed, unticked; crafted ones aren't (they
/// can be redone, so only their contribution to the totals matters).
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
                // Roll - 10% like APT, even below the tier: a roll at the
                // bottom of its tier still gets some slack, and the slider
                // reaches down to it.
                let min = tidy(round_down(value * min_fraction(), step));
                (Some(min), Some((lo.min(min), hi)), step, false)
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
/// start ticked. Other mods are listed, unticked; crafted ones aren't (they
/// can be redone, so only their contribution to the totals matters).
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
            let label = format!("Critical strike chance {crit:.2}%");
            let (min, slider, step, max_roll) = defaults(crit, None, true);
            filters.push(Filter {
                label,
                target: Target::Property("weapon_filters", "crit"),
                min,
                slider,
                step,
                max_roll,
                default_on: false,
                section: Section::Weapon,
                tier: None,
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
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: Section::Map,
        tier: None,
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

/// 5/6 links (ticked, except a rare's 5-link) and an unlinked 6-socket
/// (unticked).
fn socket_filters(item: &Item) -> Vec<Filter> {
    let mut filters = Vec::new();
    let fixed = |label: String, key: &'static str, value: u32, on: bool| {
        let value = value as f64;
        Filter::new(label, Target::Property("socket_filters", key), value, Some((value, value)), on, Section::Sockets)
    };
    if item.links >= 5 {
        let on = item.links >= 6 || item.rarity != "Rare";
        filters.push(fixed(format!("{} links", item.links), "links", item.links, on));
    }
    if item.sockets >= 6 && item.links < 6 {
        filters.push(fixed(format!("{} sockets", item.sockets), "sockets", item.sockets, false));
    }
    filters
}

/// Like APT: the item's memory strands as a minimum, ticked at 60+.
fn memory_strands_filter(item: &Item) -> Option<Filter> {
    let strands = item.memory_strands? as f64;
    Some(Filter {
        label: format!("Memory Strands {strands}"),
        target: Target::Property("misc_filters", "memory_level"),
        min: Some(strands),
        slider: Some((0.0, 100.0)),
        step: 1.0,
        max_roll: false,
        default_on: strands >= 60.0,
        section: Section::Base,
        tier: None,
        max: None,
        roll: Some(strands),
    })
}

/// Like APT for uniques: no totals, only the mods that roll (fixed ones
/// don't tell copies apart), all unticked.
fn unique_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let local = category(item).is_some_and(|(c, _)| c.starts_with("weapon.") || c.starts_with("armour."));
    let mut filters = weapon_filters(item, false);
    filters.extend(socket_filters(item));
    filters.extend(memory_strands_filter(item));
    let (options, picked) = option_filters(item, data);
    filters.extend(options);
    for (i, m) in item.mods.iter().enumerate() {
        if picked.contains(&i) {
            continue;
        }
        let (template, values) = templatize(&m.text);
        let Some((value, Some((lo, hi)))) = mod_value(&values, &m.ranges) else { continue };
        if hi <= lo {
            continue;
        }
        let Some((stat_id, sign)) = find_stat(data, m.kind, &template, local) else { continue };
        let section = kind_section(m.kind, Section::Mod);
        let mut filter = Filter::unique(mod_label(m), Target::Stat(stat_id), value * sign, (lo, hi), section);
        filter.tier = m.tier;
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
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: kind_section(kind, Section::Mod),
        tier: None,
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
    for (seen, name, pseudo, flag) in [
        (item.searing, "Searing Exarch", "Searing Exarch Implicit Modifier (#)", "searing_item"),
        (item.tangled, "Eater of Worlds", "Eater of Worlds Implicit Modifier (#)", "tangled_item"),
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
            step: 1.0,
            max_roll: false,
            default_on: on,
            section: Section::Implicit,
            tier: None,
            max: None,
            roll: tier.map(|t| t as f64),
        });
    }
    filters
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
/// shows fractured mods in gold, so only crafted mods keep a suffix.
fn mod_label(m: &ModLine) -> String {
    match m.kind {
        ModKind::Crafted => format!("{} (crafted)", m.text),
        _ => m.text.clone(),
    }
}

fn kind_section(kind: ModKind, section: Section) -> Section {
    match (kind, section) {
        (ModKind::Implicit, Section::Mod) => Section::Implicit,
        (ModKind::Enchant, Section::Mod) => Section::Enchant,
        _ => section,
    }
}

/// A mod as a stat filter of its own kind (explicit, fractured, ...).
fn mod_filter(m: &ModLine, data: &TradeData, local: bool, on: bool, section: Section) -> Option<Filter> {
    let (template, values) = templatize(&m.text);
    let (value, range) = mod_value(&values, &m.ranges)?;
    let (stat_id, sign) = find_stat(data, m.kind, &template, local)?;
    let section = kind_section(m.kind, section);
    let mut filter = Filter::new(mod_label(m), Target::Stat(stat_id), value * sign, range, on, section);
    filter.tier = m.tier;
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
            let mut filter = mod_filter(&m, data, false, true, Section::Mod)?;
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

fn has_local_mods(item: &Item) -> bool {
    category(item).is_some_and(|(c, _)| c.starts_with("weapon.") || c.starts_with("armour."))
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
        step: 1.0,
        max_roll: false,
        default_on: on,
        section: Section::Gem,
        tier: None,
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
    let local = has_local_mods(item);
    let mut filters = Vec::new();
    let flag = |label: String, key: &'static str| Filter {
        label,
        target: Target::Flag("misc_filters", key),
        min: None,
        slider: None,
        step: 1.0,
        max_roll: false,
        default_on: true,
        section: Section::Base,
        tier: None,
        max: None,
        roll: None,
    };
    filters.extend(socket_filters(item));
    if let Some(ilvl) = item.item_level {
        // Higher item levels don't add mod tiers (APT caps at 86). For a
        // linked base without crafting value, it hardly matters.
        let min = ilvl.min(86) as f64;
        filters.push(Filter {
            label: format!("Item level {ilvl}"),
            target: Target::Property("misc_filters", "ilvl"),
            min: Some(min),
            slider: Some((min.min(60.0), 86.0)),
            step: 1.0,
            max_roll: false,
            default_on: crafting_value(item),
            section: Section::Base,
            tier: None,
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
                step: 1.0,
                max_roll: false,
                default_on: on,
                section: Section::Influence(influence),
                tier: None,
                max: None,
                roll: None,
            }
        })
        .collect()
}

fn build_filters(item: &Item, data: &TradeData, preset: Preset) -> Vec<Filter> {
    if item.class == "Maps" && item.rarity != "Unique" {
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
    if preset == Preset::Base {
        return base_filters(item, data);
    }
    let local = has_local_mods(item);
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
        let Some((value, range)) = mod_value(&values, &m.ranges) else { continue };
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
        if matches!(m.kind, ModKind::Crafted | ModKind::Fractured) {
            continue;
        }
        listed.extend(mod_filter(m, data, local, false, Section::Mod));
    }

    let mut filters = Vec::new();

    filters.extend(weapon_filters(item, true));
    filters.extend(armour_filters(item));
    filters.extend(socket_filters(item));
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
        filters.push(Filter::new(label, Target::Stat(id.clone()), value, total.bounds(), on, Section::Total));
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

    filters.extend(fractured);
    filters.extend(listed);
    exact_cluster_counts(&mut filters);
    filters
}

/// One fetched listing: amount, currency id, currency name, chaos value,
/// minutes since it was listed, the item.
type Single = (f64, String, String, Option<f64>, Option<i64>, Preview);

/// A listed item for the hover preview, as (kind, text, affix tier) lines:
/// "rarity"
/// (normal/magic/rare/unique), "name", "base", "ilvl", "dps", "enchant",
/// "implicit", "fractured", "explicit", "crafted", "corrupted",
/// "unidentified".
pub type Preview = Vec<(&'static str, String, Option<u32>)>;

fn item_preview(item: &Value) -> Preview {
    let mut out: Preview = Vec::new();
    let rarity = match item["frameType"].as_u64() {
        Some(1) => "magic",
        Some(2) => "rare",
        Some(3) => "unique",
        Some(4) => "gem",
        _ => "normal",
    };
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
        ("implicit", "implicitMods"),
        ("fractured", "fracturedMods"),
        ("explicit", "explicitMods"),
        ("crafted", "craftedMods"),
    ] {
        // Plain strings, or (lately) objects with the text as "description"
        // and the affixes behind it, with tiers like "P7" (prefix tier 7).
        for m in item[key].as_array().into_iter().flatten() {
            let Some(text) = m.as_str().or(m["description"].as_str()) else { continue };
            let tier = m["mods"].as_array().into_iter().flatten().filter_map(|a| affix_tier(a["tier"].as_str()?)).min();
            // Some mods span lines ("…\nhave 20% reduced …"); the tier goes on the first.
            for (i, line) in strip_markup(text).lines().enumerate() {
                out.push((kind, line.to_string(), if i == 0 { tier } else { None }));
            }
        }
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

/// `chosen[i]`: whether filter i is used, and its bounds.
fn query(item: &Item, filters: &[Filter], chosen: &[Option<Bounds>], options: SearchOptions) -> Value {
    let mut stats = Vec::new();
    let unique = item.rarity == "Unique";
    let mut q = json!({
        "status": { "option": options.status.option() },
        "filters": { "type_filters": { "filters": { "rarity": { "option": if unique { "unique" } else { "nonunique" } } } } },
    });
    for (f, choice) in filters.iter().zip(chosen) {
        let Some(bounds) = *choice else { continue };
        match &f.target {
            Target::Stat(id) => stats.push(match bounds {
                (None, None) => json!({ "id": id }),
                _ => json!({ "id": id, "value": bounds_json(bounds) }),
            }),
            Target::StatOption(id, option) => stats.push(json!({ "id": id, "value": { "option": option } })),
            Target::Property(group, key) => q["filters"][*group]["filters"][*key] = bounds_json(bounds),
            Target::Flag(group, key) => q["filters"][*group]["filters"][*key] = json!({ "option": "true" }),
        }
    }
    q["stats"] = json!([{ "type": "and", "filters": stats }]);
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
    match options.corrupted {
        Corrupted::Any => {}
        Corrupted::No => q["filters"]["misc_filters"]["filters"]["corrupted"] = json!({ "option": "false" }),
        Corrupted::Yes => q["filters"]["misc_filters"]["filters"]["corrupted"] = json!({ "option": "true" }),
    }
    json!({ "query": q, "sort": { "price": "asc" } })
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
        let (Some(rules), Some(history)) = (self.rules.get(endpoint), self.history.get(endpoint)) else {
            return Ok(()); // limits not known yet: the first request reveals them
        };
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
            (amount, "chaos".into(), "Chaos Orb".into(), Some(amount), Some(minutes), Preview::default())
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
        ] {
            stats.insert(("explicit".to_string(), text.to_string()), id.to_string());
        }
        stats.insert(("fractured".into(), "+# to maximum Life".into()), "fractured.life".into());
        let mut transfigured = HashMap::new();
        transfigured.insert("Arc of Oscillating".to_string(), ("Arc".to_string(), "alt_y".to_string()));
        TradeData { stats, currencies: HashMap::new(), images: HashMap::new(), transfigured }
    }

    const RING: &str = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                        +25% to Cold Resistance (implicit)\n--------\n+60 to maximum Life\n+30 to Strength\n\
                        +30% to Fire Resistance\n+20 to Dexterity and Intelligence\n\
                        12% increased Rarity of Items found\n+12% to Cold Resistance (crafted)\n\
                        8% increased Rarity of Items found (crafted)\n";

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
            let opts = SearchOptions { base_only: true, corrupted: Corrupted::No, preset, status: Status::Instant };
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

    #[test]
    fn search_options() {
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let enabled: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let opts = |base_only, corrupted| SearchOptions {
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
                // 30 - 10% = 27, below the tier: the slider reaches down to it
                ("+30% total Elemental Resistance", Some(27.0), Some((27.0, 35.0)), false),
                ("12% increased Rarity of Items found", Some(10.0), Some((8.0, 14.0)), false),
            ]
        );
        // Totals sum several mods, so only listed mods carry a tier.
        let tiers: Vec<_> = filters.iter().map(|f| f.tier).collect();
        assert_eq!(tiers, vec![None, None, Some(2)]);
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
        let opts =
            SearchOptions { base_only: false, corrupted: Corrupted::No, preset: Preset::Base, status: Status::Instant };
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
            let opts = SearchOptions { base_only: false, corrupted: Corrupted::No, preset, status: Status::Instant };
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
                let opts =
                    SearchOptions { base_only: false, corrupted: Corrupted::No, preset, status: Status::Instant };
                let q = query(&item, &filters, &chosen, opts);
                assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["memory_level"]["min"], strands as f64);
            }
        }
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
        // A white 6-link: priced as a base, links ticked, item level not.
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
        assert_eq!(got[..2], [("6 links", true, Section::Sockets), ("Item level 70", false, Section::Base)]);
        assert_eq!(got.last(), Some(&("Armour 853", false, Section::Armour)));
        let chosen: Vec<Option<Bounds>> = filters.iter().map(|f| f.default_on.then_some((f.min, None))).collect();
        let opts =
            SearchOptions { base_only: true, corrupted: Corrupted::No, preset: Preset::Base, status: Status::Instant };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Astral Plate");
        assert_eq!(q["query"]["filters"]["socket_filters"]["filters"]["links"]["min"], 6.0);
        assert!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"].is_null());

        // A white 6-socket without links: a socket filter, unticked; not
        // priced as a base by default.
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
        let opts =
            SearchOptions { base_only: true, corrupted: Corrupted::No, preset: Preset::Base, status: Status::Instant };
        let q = query(&plain, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Desert Brigandine");
        assert_eq!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"]["min"], 84.0);
        let got: Vec<_> = socket_filters(&unlinked).iter().map(|f| (f.label.clone(), f.default_on)).collect();
        assert_eq!(got, vec![("6 sockets".to_string(), false)]);

        // A rare 5-link at 10% quality with local increased armour and
        // evasion: defences scaled to 20% quality, the 5-link unticked.
        let text = "Item Class: Body Armours\nRarity: Rare\nGale Shell\nTriumphant Lamellar\n--------\n\
                    Quality: +10% (augmented)\nArmour: 1000 (augmented)\nEvasion Rating: 900 (augmented)\n\
                    --------\nSockets: R-G-G-B-B \n--------\nItem Level: 84\n--------\n\
                    90% increased Armour and Evasion\n+80 to maximum Life\n";
        let item = crate::item::parse(text).unwrap();
        assert_eq!(default_preset(&item), Preset::Normal);
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let got: Vec<_> = filters.iter().map(|f| (f.label.as_str(), f.default_on, f.section)).collect();
        assert_eq!(
            got[..3],
            [
                ("Armour 1050 (at 20% quality)", false, Section::Armour),
                ("Evasion 945 (at 20% quality)", false, Section::Armour),
                ("5 links", false, Section::Sockets),
            ]
        );
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

    // ---- against replies captured by fixtures/refresh.py ----

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
        assert_eq!((f.min, f.max), (Some(19.0), Some(29.0)));
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
        let singles = parse_fetch(FETCH, &real_data(), &prices, fetch_now()).unwrap();
        assert_eq!(singles.len(), LISTINGS, "every captured listing is priced");
        for (amount, id, name, chaos, minutes, preview) in &singles {
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
        let singles = parse_fetch(FETCH, &real_data(), &prices, fetch_now()).unwrap();
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
        let mut limiter = RateLimiter::default();
        assert!(limiter.check("search").is_ok(), "limits unknown: allowed");
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
