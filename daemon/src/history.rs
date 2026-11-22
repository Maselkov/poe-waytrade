//! A price's history over the whole league, from poe.ninja, for the panel's
//! graph when its range is clicked past the week the overviews carry.
//!
//! Two kinds, named by the reference each price line carries (lookup.rs):
//!   "item:<type>:<id>"            .../economy/stash/current/item/history:
//!                                 [{daysAgo, value, count}], the value in
//!                                 the overview's primary currency (PoE1:
//!                                 chaos, PoE2: divines), count the listings
//!   "exchange:<type>:<details>"   .../economy/exchange/current/details:
//!                                 pairs, each with a history of {timestamp,
//!                                 rate}; the pair of the game's small unit
//!                                 has the rate in it
//!
//! Either becomes a day each from the oldest to today, in the small unit, a
//! day with no data left empty: what the graph draws for its week already.

use crate::ninja::{self, PriceDb};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryRef {
    Item { kind: String, id: String },
    Exchange { kind: String, details: String },
}

impl HistoryRef {
    pub fn parse(text: &str) -> Option<HistoryRef> {
        let mut parts = text.splitn(3, ':');
        let (what, kind, id) = (parts.next()?, parts.next()?, parts.next()?);
        let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !plain(kind) || !plain(id) {
            return None;
        }
        let (kind, id) = (kind.to_string(), id.to_string());
        match what {
            "item" => Some(HistoryRef::Item { kind, id }),
            "exchange" => Some(HistoryRef::Exchange { kind, details: id }),
            _ => None,
        }
    }
}

/// A value and a count a day, oldest first, the last today. Values in the
/// game's small unit; counts are listings (items only).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct History {
    pub values: Vec<Option<f64>>,
    pub counts: Vec<Option<f64>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemDay {
    days_ago: u32,
    value: f64,
    #[serde(default)]
    count: Option<f64>,
}

#[derive(Deserialize)]
struct Details {
    pairs: Vec<Pair>,
}

#[derive(Deserialize)]
struct Pair {
    id: String,
    #[serde(default)]
    history: Vec<PairDay>,
}

#[derive(Deserialize)]
struct PairDay {
    timestamp: String,
    rate: f64,
}

/// The longest history kept: poe.ninja has Standard's back a year.
const MAX_DAYS: usize = 400;

/// Fetches and reads one price's history.
pub fn fetch(db: &PriceDb, what: &HistoryRef, small: &str) -> Result<History, String> {
    let league = ninja::encode(&db.league);
    match what {
        HistoryRef::Item { kind, id } => {
            let url =
                format!("{}/economy/stash/current/item/history?league={league}&type={kind}&id={id}", ninja::base());
            let days: Vec<ItemDay> = ninja::get(&url)?;
            Ok(from_item_days(days, item_scale(db)))
        }
        HistoryRef::Exchange { kind, details } => {
            let url =
                format!("{}/economy/exchange/current/details?league={league}&type={kind}&id={details}", ninja::base());
            from_details(ninja::get(&url)?, small)
        }
    }
}

/// Small units per unit of an item history's values: PoE1's are chaos,
/// PoE2's divines (its overviews' primary currency).
fn item_scale(db: &PriceDb) -> f64 {
    match crate::game::current() {
        crate::game::Game::Poe1 => 1.0,
        crate::game::Game::Poe2 if db.divine_rate > 0.0 => 1.0 / db.divine_rate,
        crate::game::Game::Poe2 => 1.0,
    }
}

fn from_item_days(days: Vec<ItemDay>, scale: f64) -> History {
    let Some(oldest) = days.iter().map(|d| d.days_ago as usize).max() else { return History::default() };
    let len = oldest.min(MAX_DAYS - 1) + 1;
    let mut h = History { values: vec![None; len], counts: vec![None; len] };
    for d in days {
        let Some(i) = (len - 1).checked_sub(d.days_ago as usize) else { continue };
        h.values[i] = Some(d.value * scale);
        h.counts[i] = d.count;
    }
    h
}

fn from_details(details: Details, small: &str) -> Result<History, String> {
    let pair = details.pairs.into_iter().find(|p| p.id == small).ok_or(format!("no {small} rate"))?;
    let days: Vec<(i64, f64)> = pair.history.iter().filter_map(|d| Some((day_number(&d.timestamp)?, d.rate))).collect();
    let Some(newest) = days.iter().map(|d| d.0).max() else { return Ok(History::default()) };
    let oldest = days.iter().map(|d| d.0).min().unwrap_or(newest).max(newest - MAX_DAYS as i64 + 1);
    let len = (newest - oldest + 1) as usize;
    let mut h = History { values: vec![None; len], counts: vec![None; len] };
    for (day, rate) in days {
        if day >= oldest {
            h.values[(day - oldest) as usize] = Some(rate);
        }
    }
    Ok(h)
}

/// "2026-11-22T00:00:00Z" -> days since 1970-01-01.
fn day_number(timestamp: &str) -> Option<i64> {
    let date = timestamp.get(..10)?;
    let mut parts = date.split('-');
    let (y, m, d): (i64, i64, i64) =
        (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// The reply's lines: "history <v>,<v>,,<v>" and "counts <n>,,<n>".
pub fn reply_lines(h: &History) -> String {
    let join = |xs: &[Option<f64>], fmt: fn(f64) -> String| {
        xs.iter().map(|x| x.map(fmt).unwrap_or_default()).collect::<Vec<_>>().join(",")
    };
    format!("history\t{}\ncounts\t{}\n", join(&h.values, |v| format!("{v:.2}")), join(&h.counts, |c| format!("{c:.0}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(
            HistoryRef::parse("item:UniqueArmour:44311"),
            Some(HistoryRef::Item { kind: "UniqueArmour".into(), id: "44311".into() })
        );
        assert_eq!(
            HistoryRef::parse("exchange:Currency:divine-orb"),
            Some(HistoryRef::Exchange { kind: "Currency".into(), details: "divine-orb".into() })
        );
        for bad in ["", "item", "item:UniqueArmour", "thing:a:b", "item:a&b:1", "item:a:1&league=x", "item::1"] {
            assert_eq!(HistoryRef::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn days() {
        assert_eq!(day_number("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(day_number("2026-11-22T00:00:00Z").unwrap() - day_number("2026-11-14T00:00:00Z").unwrap(), 8);
        assert_eq!(day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(), 2);
        assert_eq!(day_number("nonsense"), None);
    }

    #[test]
    fn item_history() {
        let days: Vec<ItemDay> = serde_json::from_str(include_str!("../fixtures/ninja-item-history.json")).unwrap();
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../fixtures/ninja-item-history.json")).unwrap();
        let oldest = raw.iter().map(|d| d["daysAgo"].as_u64().unwrap()).max().unwrap() as usize;
        let today = raw.iter().find(|d| d["daysAgo"] == 0).unwrap();
        let h = from_item_days(days, 1.0);
        assert_eq!(h.values.len(), oldest + 1);
        assert_eq!(h.values.last().copied().flatten(), today["value"].as_f64());
        assert_eq!(h.counts.last().copied().flatten(), today["count"].as_f64());

        // PoE2: in divines, days missing.
        let days: Vec<ItemDay> =
            serde_json::from_str(include_str!("../fixtures/poe2/ninja-item-history.json")).unwrap();
        let listed = days.len();
        let h = from_item_days(days, 100.0);
        assert_eq!(h.values.iter().flatten().count(), listed);
        let raw: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../fixtures/poe2/ninja-item-history.json")).unwrap();
        let newest = raw.iter().min_by_key(|d| d["daysAgo"].as_u64().unwrap()).unwrap();
        let at = h.values.len() - 1 - newest["daysAgo"].as_u64().unwrap() as usize;
        assert_eq!(h.values[at], Some(newest["value"].as_f64().unwrap() * 100.0));
    }

    #[test]
    fn exchange_history() {
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/ninja-exchange-details.json")).unwrap();
        let chaos = raw["pairs"].as_array().unwrap().iter().find(|p| p["id"] == "chaos").unwrap();
        let days = chaos["history"].as_array().unwrap();
        let newest = days.iter().max_by_key(|d| d["timestamp"].as_str().unwrap().to_string()).unwrap();
        let h = from_details(serde_json::from_value(raw.clone()).unwrap(), "chaos").unwrap();
        assert!(h.values.len() >= days.len());
        assert_eq!(h.values.last().copied().flatten(), newest["rate"].as_f64());
        assert!(h.counts.iter().all(Option::is_none));
        assert!(from_details(serde_json::from_value(raw).unwrap(), "exalted").is_err());

        // PoE2: the exalted pair.
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/poe2/ninja-exchange-details.json")).unwrap();
        let h = from_details(serde_json::from_value(raw).unwrap(), "exalted").unwrap();
        assert!(h.values.iter().flatten().count() > 0);
    }

    #[test]
    fn reply() {
        let h = History { values: vec![Some(1.5), None, Some(30.0)], counts: vec![Some(3.0), None, None] };
        assert_eq!(reply_lines(&h), "history\t1.50,,30.00\ncounts\t3,,\n");
    }
}
