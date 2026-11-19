//! Matching a parsed item against poe.ninja data.

use crate::item::Item;
use crate::ninja::{ItemLine, PriceDb};
use crate::trade::templatize;

pub struct Price {
    pub label: String,
    pub chaos: f64,
    pub divine: f64,
    pub listings: Option<u32>,
}

pub enum Outcome {
    Found { title: String, prices: Vec<Price> },
    NotFound(String),
    Unsupported(String),
}

/// Most variants worth showing for one item (e.g. a unique's rolls).
const MAX_PRICES: usize = 4;

pub fn lookup(db: &PriceDb, item: &Item) -> Outcome {
    let title = item.name.clone().unwrap_or_else(|| item.base.clone());

    if item.rarity == "Gem" {
        return match db.items.get(&item.base).and_then(|lines| best_gem(lines, item)) {
            Some(line) => found(title, vec![line_price(db, line)]),
            None => Outcome::NotFound(format!("No poe.ninja price for {}", item.base)),
        };
    }

    if item.rarity == "Unique" {
        let lines = match &item.name {
            Some(name) => db.items.get(name),
            None => db.uniques_by_base.get(&item.base), // unidentified
        };
        return match lines.map(|l| unique_prices(db, l, item)) {
            Some(prices) if !prices.is_empty() => found(title, prices),
            _ => Outcome::NotFound(format!("No poe.ninja price for {title}")),
        };
    }

    // Currency, fragments, essences, divination cards, omens, ...
    if let Some(&chaos) = db.exchange.get(&item.base) {
        let mut prices = vec![Price { label: "each".to_string(), chaos, divine: db.to_divine(chaos), listings: None }];
        if let Some(stack) = item.stack_size.filter(|&n| n > 1) {
            let total = chaos * stack as f64;
            prices.push(Price {
                label: format!("stack of {stack}"),
                chaos: total,
                divine: db.to_divine(total),
                listings: None,
            });
        }
        return found(title, prices);
    }

    // Wickerman limbs: one line per limb and implicit, the implicits'
    // templates as the variant ("+# to Strength", joined by " / ").
    if item.class == "Viscera" {
        return match db.items.get(&item.base) {
            Some(lines) => found(title, viscera_prices(db, lines, item)),
            None => Outcome::NotFound(format!("No poe.ninja price for {title}")),
        };
    }

    // Maps, invitations, beasts: listed by name.
    let named = item.name.as_ref().and_then(|n| db.items.get(n));
    if let Some(lines) = named.or_else(|| db.items.get(&item.base)) {
        let mut prices: Vec<Price> = lines.iter().map(|l| line_price(db, l)).collect();
        prices.truncate(MAX_PRICES);
        return found(title, prices);
    }

    if matches!(item.rarity.as_str(), "Rare" | "Magic" | "Normal") {
        return Outcome::Unsupported("Rare and magic items need a trade search (not implemented yet)".into());
    }
    Outcome::NotFound(format!("No poe.ninja price for {title}"))
}

fn found(title: String, prices: Vec<Price>) -> Outcome {
    Outcome::Found { title, prices }
}

fn line_price(db: &PriceDb, line: &ItemLine) -> Price {
    let mut label = line.variant.clone().unwrap_or_default();
    if let Some(links) = line.links.filter(|&l| l >= 5) {
        label = if label.is_empty() { format!("{links}L") } else { format!("{label}, {links}L") };
    }
    Price {
        label,
        chaos: line.chaos_value,
        divine: line.divine_value.unwrap_or_else(|| db.to_divine(line.chaos_value)),
        listings: line.listing_count,
    }
}

/// poe.ninja lists 5L/6L uniques separately; anything below 5 links is the
/// unlinked line. Variants (different rolls of one unique) are all shown.
fn unique_prices(db: &PriceDb, lines: &[ItemLine], item: &Item) -> Vec<Price> {
    let want = if item.links >= 5 { Some(item.links) } else { None };
    let mut matching: Vec<&ItemLine> = lines.iter().filter(|l| l.links.filter(|&n| n >= 5) == want).collect();
    if matching.is_empty() {
        matching = lines.iter().collect();
    }
    // Most-listed first: those prices are the most trustworthy.
    matching.sort_by_key(|l| std::cmp::Reverse(l.listing_count.unwrap_or(0)));
    matching
        .into_iter()
        .take(MAX_PRICES)
        .map(|l| {
            let mut price = line_price(db, l);
            // Unidentified: several uniques share the base, so say which.
            if item.name.is_none() {
                price.label =
                    if price.label.is_empty() { l.name.clone() } else { format!("{} ({})", l.name, price.label) };
            }
            price
        })
        .collect()
}

/// The line for this limb's implicits, or the most-listed ones if poe.ninja
/// has none for exactly these.
fn viscera_prices(db: &PriceDb, lines: &[ItemLine], item: &Item) -> Vec<Price> {
    let want = item.mods.iter().map(|m| templatize(&m.text).0).collect::<Vec<_>>().join(" / ");
    if let Some(line) = lines.iter().find(|l| l.variant.as_deref() == Some(want.as_str())) {
        let mut price = line_price(db, line);
        price.label.clear(); // the panel already lists the mods
        return vec![price];
    }
    let mut sorted: Vec<&ItemLine> = lines.iter().collect();
    sorted.sort_by_key(|l| std::cmp::Reverse(l.listing_count.unwrap_or(0)));
    sorted.into_iter().take(MAX_PRICES).map(|l| line_price(db, l)).collect()
}

/// Same corruption state, closest level, then the highest quality bucket not
/// above the gem's own quality.
fn best_gem<'a>(lines: &'a [ItemLine], item: &Item) -> Option<&'a ItemLine> {
    let level = item.gem_level.unwrap_or(1);
    let same_corruption: Vec<&ItemLine> =
        lines.iter().filter(|l| l.corrupted.unwrap_or(false) == item.corrupted).collect();
    let pool = if same_corruption.is_empty() { lines.iter().collect() } else { same_corruption };
    let closest = pool.iter().map(|l| l.gem_level.unwrap_or(1).abs_diff(level)).min()?;
    pool.into_iter().filter(|l| l.gem_level.unwrap_or(1).abs_diff(level) == closest).min_by_key(|l| {
        let q = l.gem_quality.unwrap_or(0);
        // Buckets at or below our quality first (closest wins), then above.
        if q <= item.quality { (0, item.quality - q) } else { (1, q - item.quality) }
    })
}
