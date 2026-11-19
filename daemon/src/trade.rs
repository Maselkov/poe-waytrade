//! Official trade site search for rare/magic items, sent through the
//! headless browser (see cdp.rs for why).

use crate::cdp::{self, Browser, HttpResponse};
use crate::item::{Item, ModKind};
use crate::ninja::{self, PriceDb};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// Stat/currency reference data is refetched after this long.
const DATA_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// The browser is closed after this long without a search.
pub const BROWSER_IDLE: Duration = Duration::from_secs(10 * 60);
/// Default filter minimum, as a fraction of the item's own roll.
const MIN_FRACTION: f64 = 0.85;
/// Listings fetched per search (one fetch request allows up to 10).
const LISTINGS: usize = 10;

pub struct Filter {
    pub label: String,
    pub stat_id: String,
    pub min: Option<f64>,
}

pub struct Listing {
    pub amount: f64,
    pub currency: String,
    pub chaos: Option<f64>,
    /// "2m", "3h", "5d" since listed; empty if unknown.
    pub age: String,
}

pub struct TradeResult {
    pub filters: Vec<Filter>,
    pub enabled: Vec<bool>,
    pub listings: Vec<Listing>,
    pub total: u64,
    pub url: String,
}

struct TradeData {
    /// (stat group, text template) -> trade stat id
    stats: HashMap<(String, String), String>,
    /// currency id ("divine") -> name ("Divine Orb")
    currencies: HashMap<String, String>,
}

pub struct Trade {
    browser: Option<Browser>,
    data: Option<(TradeData, Instant)>,
    limiter: RateLimiter,
    last_used: Instant,
}

/// Items worth a trade search: anything non-unique with mods.
pub fn searchable(item: &Item) -> bool {
    matches!(item.rarity.as_str(), "Rare" | "Magic" | "Normal") && !item.mods.is_empty()
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

    /// `enabled`: indices of filters to use; `None` = all.
    pub fn search(
        &mut self,
        item: &Item,
        enabled: Option<&[usize]>,
        league: &str,
        prices: &PriceDb,
    ) -> Result<TradeResult, String> {
        self.last_used = Instant::now();
        let result = self.search_inner(item, enabled, league, prices);
        // A dead or confused browser gets relaunched next time.
        if result.is_err() && !self.browser.as_mut().is_some_and(|b| b.alive()) {
            self.browser = None;
        }
        result
    }

    fn search_inner(
        &mut self,
        item: &Item,
        enabled: Option<&[usize]>,
        league: &str,
        prices: &PriceDb,
    ) -> Result<TradeResult, String> {
        self.ensure_data()?;
        let data = &self.data.as_ref().unwrap().0;
        let filters = build_filters(item, data);
        if filters.is_empty() {
            return Err("No mods on this item match trade search stats".into());
        }
        let enabled: Vec<bool> = match enabled {
            Some(idx) => (0..filters.len()).map(|i| idx.contains(&i)).collect(),
            None => vec![true; filters.len()],
        };
        let body = query(item, &filters, &enabled).to_string();
        let league_path = ninja::encode(league);

        let resp = self.request("search", "POST", &format!("/api/trade/search/{league_path}"), Some(&body))?;
        let found: Value = serde_json::from_str(&resp.body).map_err(|e| format!("bad search reply: {e}"))?;
        let id = found["id"].as_str().ok_or("search reply has no id")?.to_string();
        let total = found["total"].as_u64().unwrap_or(0);
        let hashes: Vec<&str> =
            found["result"].as_array().map(|a| a.iter().filter_map(Value::as_str).take(LISTINGS).collect()).unwrap_or_default();

        let mut listings = Vec::new();
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
                let chaos = if currency == "chaos" {
                    Some(amount)
                } else {
                    prices.exchange.get(&name).map(|c| c * amount)
                };
                listings.push(Listing {
                    amount,
                    currency: name,
                    chaos,
                    age: listing["indexed"].as_str().and_then(listing_age).unwrap_or_default(),
                });
            }
        }

        Ok(TradeResult {
            filters,
            enabled,
            listings,
            total,
            url: format!("https://www.pathofexile.com/trade/search/{league_path}/{id}"),
        })
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
    fn request(&mut self, endpoint: &'static str, method: &str, path: &str, body: Option<&str>) -> Result<HttpResponse, String> {
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

        let mut data = TradeData { stats: HashMap::new(), currencies: HashMap::new() };
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
        if fresh {
            if let Some(v) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                return Ok(v);
            }
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
    base.join("poe-overlay")
}

/// "2026-11-18T22:01:33Z" -> how long ago, as "2m" / "3h" / "5d".
fn listing_age(indexed: &str) -> Option<String> {
    let then = parse_utc(indexed)?;
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    let minutes = (now - then).max(0) / 60;
    Some(match minutes {
        m if m < 60 => format!("{m}m"),
        m if m < 48 * 60 => format!("{}h", m / 60),
        m => format!("{}d", m / (24 * 60)),
    })
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

fn category(item: &Item) -> Option<&'static str> {
    if item.base.contains("Cluster Jewel") {
        return Some("jewel.cluster");
    }
    Some(match item.class.as_str() {
        "Body Armours" => "armour.chest",
        "Boots" => "armour.boots",
        "Gloves" => "armour.gloves",
        "Helmets" => "armour.helmet",
        "Shields" => "armour.shield",
        "Quivers" => "armour.quiver",
        "Amulets" => "accessory.amulet",
        "Belts" => "accessory.belt",
        "Rings" => "accessory.ring",
        "Jewels" => "jewel.base",
        "Abyss Jewels" => "jewel.abyss",
        "Bows" => "weapon.bow",
        "Claws" => "weapon.claw",
        "Daggers" => "weapon.basedagger",
        "Rune Daggers" => "weapon.runedagger",
        "One Hand Axes" => "weapon.oneaxe",
        "One Hand Swords" => "weapon.basesword",
        "Thrusting One Hand Swords" => "weapon.rapier",
        "One Hand Maces" => "weapon.basemace",
        "Sceptres" => "weapon.sceptre",
        "Staves" => "weapon.basestaff",
        "Warstaves" => "weapon.warstaff",
        "Two Hand Axes" => "weapon.twoaxe",
        "Two Hand Maces" => "weapon.twomace",
        "Two Hand Swords" => "weapon.twosword",
        "Wands" => "weapon.wand",
        c if c.ends_with("Flasks") => "flask",
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
    candidates
        .into_iter()
        .find_map(|(t, sign)| data.stats.get(&(group.clone(), t)).map(|id| (id.clone(), sign)))
}

fn build_filters(item: &Item, data: &TradeData) -> Vec<Filter> {
    let local = category(item).is_some_and(|c| c.starts_with("weapon.") || c.starts_with("armour."));
    let mut filters = Vec::new();
    let (mut elemental, mut chaos) = (0.0, 0.0);

    for m in &item.mods {
        let (template, values) = templatize(&m.text);
        let value = match values.as_slice() {
            [] => continue,
            [a, b] => (a + b) / 2.0, // "Adds # to # ..." is searched by average
            [v, ..] => *v,
        };
        // Resistances are searched as totals, like on the trade site's
        // pseudo filters, so it doesn't matter which mod they come from.
        if let Some((elements, has_chaos)) = resist_parts(&template) {
            elemental += value * elements as f64;
            if has_chaos {
                chaos += value;
            }
            continue;
        }
        let Some((stat_id, sign)) = find_stat(data, m.kind, &template, local) else { continue };
        let value = value * sign;
        let label = match m.kind {
            ModKind::Explicit => m.text.clone(),
            kind => format!("{} ({})", m.text, kind.trade_group()),
        };
        filters.push(Filter { label, stat_id, min: min_for(value) });
    }

    if elemental > 0.0 {
        filters.push(Filter {
            label: format!("+{elemental}% total Elemental Resistance"),
            stat_id: "pseudo.pseudo_total_elemental_resistance".into(),
            min: min_for(elemental),
        });
    }
    if chaos > 0.0 {
        filters.push(Filter {
            label: format!("+{chaos}% total Chaos Resistance"),
            stat_id: "pseudo.pseudo_total_chaos_resistance".into(),
            min: min_for(chaos),
        });
    }
    filters
}

fn min_for(value: f64) -> Option<f64> {
    (value > 0.0).then(|| (value * MIN_FRACTION).floor())
}

fn query(item: &Item, filters: &[Filter], enabled: &[bool]) -> Value {
    let stats: Vec<Value> = filters
        .iter()
        .zip(enabled)
        .filter(|(_, on)| **on)
        .map(|(f, _)| match f.min {
            Some(min) => json!({ "id": f.stat_id, "value": { "min": min } }),
            None => json!({ "id": f.stat_id }),
        })
        .collect();
    let mut q = json!({
        "status": { "option": "online" },
        "stats": [{ "type": "and", "filters": stats }],
        "filters": { "type_filters": { "filters": { "rarity": { "option": "nonunique" } } } },
    });
    match category(item) {
        Some(c) => q["filters"]["type_filters"]["filters"]["category"] = json!({ "option": c }),
        None => q["type"] = json!(item.base),
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
            for part in resp.headers.get(&format!("x-rate-limit-{rule}-state")).map(String::as_str).unwrap_or("").split(',') {
                let f: Vec<u64> = part.split(':').filter_map(|n| n.parse().ok()).collect();
                if let [_, _, penalty, ..] = f[..] {
                    if penalty > 0 {
                        self.block_for(Duration::from_secs(penalty));
                    }
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
    fn timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-18T22:01:33Z"), Some(1795039293));
        assert_eq!(parse_utc("nonsense"), None);
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
