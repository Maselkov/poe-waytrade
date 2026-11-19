//! Official trade site search for rare/magic items, sent through the
//! headless browser (see cdp.rs for why).

use crate::cdp::{self, Browser, HttpResponse};
use crate::item::{Item, ModKind, ModLine};
use crate::ninja::{self, PriceDb};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Stat/currency reference data is refetched after this long.
const DATA_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The browser is closed after this long without a search.
pub const BROWSER_IDLE: Duration = Duration::from_secs(10 * 60);
/// Default filter minimum, as a fraction of the item's own roll (APT's
/// default range is 10%).
const MIN_FRACTION: f64 = 0.9;
/// Listings fetched per search (one fetch request allows up to 10).
const LISTINGS: usize = 10;

/// What a filter searches: a stat (by trade id) or an item property such
/// as weapon DPS (by its group and key in the query's filters).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Stat(String),
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
    /// Base item properties: item level, influences, fractured/synthesised.
    Base,
    /// Armour, evasion, energy shield, ward, block.
    Armour,
    /// Links and sockets.
    Sockets,
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
            Section::Armour => "armour",
            Section::Sockets => "sockets",
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
        Filter { label, target, min, slider, step, max_roll, default_on: on, section, tier: None, roll: Some(value) }
    }

    /// A unique's rolled mod, unticked. Like APT, the minimum is the roll
    /// minus 10% of the mod's range (a perfect roll: exactly the roll).
    fn unique(label: String, target: Target, value: f64, (lo, hi): (f64, f64), section: Section) -> Filter {
        let mut f = Filter::new(label, target, value, Some((lo, hi)), false, section);
        if let (Some(_), false) = (f.min, f.max_roll) {
            let min =
                ((round_down(value - (hi - lo) * (1.0 - MIN_FRACTION), f.step)) / f.step).round() / (1.0 / f.step);
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

/// How to search, besides the stat filters.
#[derive(Clone, Copy, Debug)]
pub struct SearchOptions {
    /// The exact base type ("Sapphire Ring") rather than its category.
    pub base_only: bool,
    pub corrupted: Corrupted,
    pub preset: Preset,
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
/// Also offered for any 5- or 6-link, which sells as a linked base.
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
    if item.links >= 5 && matches!(item.rarity.as_str(), "Normal" | "Magic" | "Rare") {
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
/// worth its links and base, not its mods.
pub fn default_preset(item: &Item) -> Preset {
    if item.links >= 5 && matches!(item.rarity.as_str(), "Normal" | "Magic") && base_preset_available(item) {
        Preset::Base
    } else {
        Preset::Normal
    }
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
}

pub struct Trade {
    browser: Option<Browser>,
    data: Option<(TradeData, Instant)>,
    limiter: RateLimiter,
    last_used: Instant,
}

/// Items worth a trade search: anything non-unique with mods.
pub fn searchable(item: &Item) -> bool {
    match item.rarity.as_str() {
        "Rare" | "Magic" | "Normal" => !item.mods.is_empty() || item.links >= 5,
        // Identified uniques: searched by name, with their rolls.
        "Unique" => item.name.is_some() && !item.mods.is_empty(),
        _ => false,
    }
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
        let corrupted = if item.corrupted { Corrupted::Any } else { Corrupted::No };
        Ok(Prepared {
            filters,
            category_label,
            base_preset: base_preset_available(item),
            defaults: SearchOptions { base_only: preset == Preset::Base, corrupted, preset },
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
        let body = query(item, &filters, &chosen, options).to_string();
        let league_path = ninja::encode(league);

        let resp = self.request("search", "POST", &format!("/api/trade/search/{league_path}"), Some(&body))?;
        let found: Value = serde_json::from_str(&resp.body).map_err(|e| format!("bad search reply: {e}"))?;
        let id = found["id"].as_str().ok_or("search reply has no id")?.to_string();
        let total = found["total"].as_u64().unwrap_or(0);
        let hashes: Vec<&str> = found["result"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).take(LISTINGS).collect())
            .unwrap_or_default();

        // One entry per listing first.
        let mut singles: Vec<Single> = Vec::new();
        if !hashes.is_empty() {
            let path = format!("/api/trade/fetch/{}?query={id}", hashes.join(","));
            let resp = self.request("fetch", "GET", &path, None)?;
            let fetched: Value = serde_json::from_str(&resp.body).map_err(|e| format!("bad fetch reply: {e}"))?;
            let data = &self.data.as_ref().unwrap().0;
            for entry in fetched["result"].as_array().into_iter().flatten() {
                let listing = &entry["listing"];
                let (Some(amount), Some(currency)) =
                    (listing["price"]["amount"].as_f64(), listing["price"]["currency"].as_str())
                else {
                    continue; // unpriced
                };
                let name = data.currencies.get(currency).cloned().unwrap_or_else(|| currency.to_string());
                let chaos =
                    if currency == "chaos" { Some(amount) } else { prices.exchange.get(&name).map(|c| c * amount) };
                let minutes = listing["indexed"].as_str().and_then(listing_minutes);
                singles.push((amount, currency.to_string(), name, chaos, minutes, item_preview(&entry["item"])));
            }
        }

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
                Some(last) if last.amount == amount && last.currency_id == id => {
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

        Ok(TradeResult {
            listings,
            summary,
            total,
            url: format!("https://www.pathofexile.com/trade/search/{league_path}/{id}"),
        })
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
        let resp = self.browser()?.fetch(method, path, body)?;
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

        let mut data = TradeData { stats: HashMap::new(), currencies: HashMap::new(), images: HashMap::new() };
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

/// "2026-11-18T22:01:33Z" -> minutes ago.
fn listing_minutes(indexed: &str) -> Option<i64> {
    let then = parse_utc(indexed)?;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((now - then).max(0) / 60)
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
                let min = tidy(round_down(value * MIN_FRACTION, step));
                (Some(min), Some((lo.min(min), hi)), step, false)
            }
        }
        // A fixed roll: nothing to slide.
        Some(_) => (Some(tidy(value)), None, step, false),
        None => {
            let lo = round_down(value * 0.5, step);
            let hi = ((value * 1.25) / step).ceil() * step;
            (Some(tidy(round_down(value * MIN_FRACTION, step))), Some((lo, hi)), step, false)
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

/// Like APT for uniques: no totals, only the mods that roll (fixed ones
/// don't tell copies apart), all unticked.
fn unique_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let local = category(item).is_some_and(|(c, _)| c.starts_with("weapon.") || c.starts_with("armour."));
    let mut filters = weapon_filters(item, false);
    filters.extend(socket_filters(item));
    for m in &item.mods {
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

fn has_local_mods(item: &Item) -> bool {
    category(item).is_some_and(|(c, _)| c.starts_with("weapon.") || c.starts_with("armour."))
}

/// APT's "base item" preset: what makes the item worth as a crafting base.
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
            roll: Some(ilvl as f64),
        });
    }
    if item.fractured_item {
        filters.push(flag("Fractured item".into(), "fractured_item"));
    }
    if item.synthesised {
        filters.push(flag("Synthesised item".into(), "synthesised_item"));
    }
    for &influence in &item.influences {
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
        filters.push(flag(format!("{name} influence"), key));
    }
    // Influenced and fractured bases' implicits vary too much to match; a
    // linked base without crafting value is worth its links, not its rolls.
    let implicits = item.influences.is_empty() && !item.fractured_item;
    let valuable = crafting_value(item);
    for m in &item.mods {
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
    filters
}

fn build_filters(item: &Item, data: &TradeData, preset: Preset) -> Vec<Filter> {
    if item.rarity == "Unique" {
        return unique_filters(item, data);
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

    for m in &item.mods {
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
        _ => "normal",
    };
    out.push(("rarity", rarity.to_string(), None));
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

/// `chosen[i]`: whether filter i is used, and its bounds.
fn query(item: &Item, filters: &[Filter], chosen: &[Option<Bounds>], options: SearchOptions) -> Value {
    let mut stats = Vec::new();
    let unique = item.rarity == "Unique";
    let mut q = json!({
        "status": { "option": "online" },
        "filters": { "type_filters": { "filters": { "rarity": { "option": if unique { "unique" } else { "nonunique" } } } } },
    });
    for (f, choice) in filters.iter().zip(chosen) {
        let Some(bounds) = *choice else { continue };
        match &f.target {
            Target::Stat(id) => stats.push(match bounds {
                (None, None) => json!({ "id": id }),
                _ => json!({ "id": id, "value": bounds_json(bounds) }),
            }),
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
            _ => q["type"] = json!(item.base),
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
        TradeData { stats, currencies: HashMap::new(), images: HashMap::new() }
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

    #[test]
    fn search_options() {
        let item = crate::item::parse(RING).unwrap();
        let filters = build_filters(&item, &test_data(), Preset::Normal);
        let enabled: Vec<Option<Bounds>> = filters.iter().map(|f| Some((f.min, None))).collect();
        let opts = |base_only, corrupted| SearchOptions { base_only, corrupted, preset: Preset::Normal };

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
            SearchOptions { base_only: false, corrupted: Corrupted::No, preset: Preset::Normal },
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
        let opts = SearchOptions { base_only: false, corrupted: Corrupted::No, preset: Preset::Base };
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
        let opts = SearchOptions { base_only: true, corrupted: Corrupted::No, preset: Preset::Base };
        let q = query(&item, &filters, &chosen, opts);
        assert_eq!(q["query"]["type"], "Astral Plate");
        assert_eq!(q["query"]["filters"]["socket_filters"]["filters"]["links"]["min"], 6.0);
        assert!(q["query"]["filters"]["misc_filters"]["filters"]["ilvl"].is_null());

        // A white 6-socket without links: a socket filter, unticked; not
        // priced as a base by default.
        let unlinked = crate::item::parse(&text.replace("R-R-G-G-B-B", "R R G G B B")).unwrap();
        assert_eq!(default_preset(&unlinked), Preset::Normal);
        assert!(!searchable(
            &crate::item::parse(
                &text
                    .replace("+12% to all Elemental Resistances (implicit)\n", "")
                    .replace("R-R-G-G-B-B", "R R G G B B")
            )
            .unwrap()
        ));
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
            SearchOptions { base_only: false, corrupted: Corrupted::Any, preset: Preset::Normal },
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
            SearchOptions { base_only: false, corrupted: Corrupted::Any, preset: Preset::Normal },
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
}
