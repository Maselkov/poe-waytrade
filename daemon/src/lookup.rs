//! Matching a parsed item against poe.ninja data.

use crate::item::Item;
use crate::ninja::{ItemLine, PriceDb};
use crate::trade::templatize;

#[derive(Debug)]
pub struct Price {
    pub label: String,
    pub chaos: f64,
    pub divine: f64,
    pub listings: Option<u32>,
    /// The unique this price is for, on an unidentified unique's list of
    /// candidates: the panel checks the item as that one when it's clicked.
    pub unique: Option<String>,
}

pub enum Outcome {
    Found { title: String, prices: Vec<Price> },
    NotFound(String),
    Unsupported(String),
}

/// Heist's experimented base types (poedb's list of 40). Whatever their
/// mods, they sell as their base: priced by poe.ninja's base price.
pub const EXPERIMENTED_BASES: &[&str] = &[
    "Astrolabe Amulet",
    "Simplex Amulet",
    "Cogwork Ring",
    "Geodesic Ring",
    "Micro-Distillery Belt",
    "Accumulator Wand",
    "Congregator Wand",
    "Alternating Sceptre",
    "Oscillating Sceptre",
    "Stabilising Sceptre",
    "Boom Mace",
    "Crack Mace",
    "Crushing Force Magnifier",
    "Impact Force Propagator",
    "Disapprobation Axe",
    "Psychotic Axe",
    "Apex Cleaver",
    "Honed Cleaver",
    "Anarchic Spiritblade",
    "Capricious Spiritblade",
    "Banishing Blade",
    "Blasting Blade",
    "Flashfire Blade",
    "Infernal Blade",
    "Pneumatic Dagger",
    "Pressurised Dagger",
    "Malign Fangs",
    "Void Fangs",
    "Battery Staff",
    "Eventuality Rod",
    "Potentiality Rod",
    "Reciprocation Staff",
    "Foundry Bow",
    "Solarine Bow",
    "Cold-attuned Buckler",
    "Heat-attuned Tower Shield",
    "Magmatic Tower Shield",
    "Polar Buckler",
    "Subsuming Spirit Shield",
    "Transfer-attuned Spirit Shield",
];

/// An experimented base's poe.ninja price, by its item level and influences
/// when known; without an item level, its line at item level 82 without
/// influences (what a buyer would assume, and the cheapest).
pub fn experimented_price(db: &PriceDb, item: &Item) -> Option<Price> {
    if !EXPERIMENTED_BASES.contains(&item.base.as_str()) {
        return None;
    }
    if item.item_level.is_some() {
        let plain = Item { rarity: "Normal".into(), corrupted: false, ..clone_base(item) };
        if let Some(p) = base_prices(db, &plain).into_iter().next() {
            return Some(p);
        }
    }
    let lines = db.bases.get(&item.base)?;
    let line = lines.iter().filter(|l| l.variant.is_none()).min_by_key(|l| l.level_required.unwrap_or(99))?;
    let mut price = line_price(db, line);
    price.label = format!("ilvl {}", line.level_required.unwrap_or(82));
    Some(price)
}

/// The fields base prices go by.
fn clone_base(item: &Item) -> Item {
    Item {
        class: item.class.clone(),
        base: item.base.clone(),
        item_level: item.item_level,
        influences: item.influences.clone(),
        ..Default::default()
    }
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
        return match lines.map(|l| unique_prices(db, l, item, item.name.is_none())) {
            Some(prices) if !prices.is_empty() => found(title, prices),
            _ => Outcome::NotFound(format!("No poe.ninja price for {title}")),
        };
    }

    // A Facetor's Lens: poe.ninja has no price by stored experience.
    if item.stored_experience.is_some() {
        return Outcome::Unsupported("Priced by stored experience on the trade site".into());
    }

    // Currency, fragments, essences, divination cards, omens, ...
    if let Some(&chaos) = db.exchange.get(&item.base) {
        let mut prices =
            vec![Price { label: "each".to_string(), chaos, divine: db.to_divine(chaos), listings: None, unique: None }];
        if let Some(stack) = item.stack_size.filter(|&n| n > 1) {
            let total = chaos * stack as f64;
            prices.push(Price {
                label: format!("stack of {stack}"),
                chaos: total,
                divine: db.to_divine(total),
                listings: None,
                unique: None,
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
        return Outcome::Unsupported("No trade search for this kind of item".into());
    }
    Outcome::NotFound(format!("No poe.ninja price for {title}"))
}

/// poe.ninja's price for what an unfinished item is worth as a base, shown
/// next to its trade filters: plain bases of item level 82+ (by item level
/// and influences) and white or magic cluster jewels (by enchant, passive
/// count and item level bracket). Empty for anything else.
pub fn base_prices(db: &PriceDb, item: &Item) -> Vec<Price> {
    if item.base.contains("Cluster Jewel") && matches!(item.rarity.as_str(), "Normal" | "Magic") {
        return cluster_price(db, item).into_iter().collect();
    }
    let plain = item.rarity == "Normal" && !item.corrupted && !item.fractured_item && !item.synthesised;
    let Some(ilvl) = item.item_level.filter(|&l| plain && l >= 82) else { return Vec::new() };
    let Some(lines) = db.bases.get(&item.base) else { return Vec::new() };
    // "Shaper/Elder" or "Warlord/Hunter": poe.ninja's order varies.
    let mut want: Vec<String> = item.influences.iter().map(|i| i.to_string()).collect();
    want.sort();
    let line = lines.iter().find(|l| {
        let mut have: Vec<String> =
            l.variant.iter().flat_map(|v| v.split('/')).map(|s| s.trim().to_lowercase()).collect();
        have.sort();
        l.level_required == Some(ilvl.min(86)) && have == want
    });
    line.map(|l| {
        let mut price = line_price(db, l);
        let level = if ilvl >= 86 { "86+".to_string() } else { ilvl.to_string() };
        price.label = match &l.variant {
            Some(v) => format!("ilvl {level} {}", v.replace('/', " + ")),
            None => format!("ilvl {level}"),
        };
        price
    })
    .into_iter()
    .collect()
}

/// "Adds 8 Passive Skills" and "Added Small Passive Skills grant: ..."
/// (one line per grant; poe.ninja joins them with ", ").
fn cluster_price(db: &PriceDb, item: &Item) -> Option<Price> {
    let passives = item.mods.iter().find_map(|m| {
        m.text
            .strip_prefix("Adds ")?
            .strip_suffix(" Passive Skills")
            .or_else(|| m.text.strip_prefix("Adds ")?.strip_suffix(" Passive Skill"))?
            .parse::<u32>()
            .ok()
    })?;
    let grants: Vec<&str> =
        item.mods.iter().filter_map(|m| m.text.strip_prefix("Added Small Passive Skills grant: ")).collect();
    if grants.is_empty() {
        return None;
    }
    let enchant = grants.join(", ");
    let bracket = cluster_bracket(item.item_level.unwrap_or(1));
    let variant = format!("{passives} passive{}", if passives == 1 { "" } else { "s" });
    let lines = db.clusters.get(&item.base)?;
    let line = lines
        .iter()
        .filter(|l| l.name == enchant && l.variant.as_deref() == Some(variant.as_str()))
        // Its own bracket, or the best one below it.
        .filter(|l| l.level_required.unwrap_or(1) <= bracket)
        .max_by_key(|l| l.level_required.unwrap_or(1))?;
    let mut price = line_price(db, line);
    price.label = format!("ilvl {}+", line.level_required.unwrap_or(1));
    Some(price)
}

/// The item level brackets clusters are sold by (they decide which notables
/// can roll): 1, 50, 68, 75 and 84.
fn cluster_bracket(ilvl: u32) -> u32 {
    [84, 75, 68, 50].into_iter().find(|&b| ilvl >= b).unwrap_or(1)
}

/// An unidentified unique once its art says which unique it is: only the
/// lines of `names` (several when uniques share their art, like a
/// Foulborn one and its original), named only if there are several.
pub fn unique_prices_named(db: &PriceDb, item: &Item, names: &[String]) -> Outcome {
    let lines: Vec<ItemLine> = db
        .uniques_by_base
        .get(&item.base)
        .map(|lines| lines.iter().filter(|l| names.contains(&l.name)).cloned().collect())
        .unwrap_or_default();
    let prices = unique_prices(db, &lines, item, names.len() > 1);
    if prices.is_empty() {
        return Outcome::NotFound(format!("No poe.ninja price for {}", names.join(" / ")));
    }
    found(names.join(" / "), prices)
}

/// An unidentified unique's candidates by art: (art URL, names of the
/// uniques with that art), in poe.ninja's order. Empty unless there's more
/// than one art to tell apart.
pub fn unique_arts(db: &PriceDb, item: &Item) -> Vec<(String, Vec<String>)> {
    if item.rarity != "Unique" || item.name.is_some() {
        return Vec::new();
    }
    let mut arts: Vec<(String, Vec<String>)> = Vec::new();
    for line in db.uniques_by_base.get(&item.base).into_iter().flatten() {
        let Some(icon) = &line.icon else { return Vec::new() }; // can't rule that one out
        match arts.iter_mut().find(|(url, _)| url == icon) {
            Some((_, names)) if names.contains(&line.name) => {}
            Some((_, names)) => names.push(line.name.clone()),
            None => arts.push((icon.clone(), vec![line.name.clone()])),
        }
    }
    if arts.len() < 2 { Vec::new() } else { arts }
}

/// Every art an art match could need: those of the uniques on bases where
/// uniques look different (see unique_arts), for fetching ahead of time.
pub fn matchable_arts(db: &PriceDb) -> Vec<String> {
    let mut out = Vec::new();
    for lines in db.uniques_by_base.values() {
        let mut arts: Vec<&String> = lines.iter().filter_map(|l| l.icon.as_ref()).collect();
        arts.sort();
        arts.dedup();
        if arts.len() >= 2 {
            out.extend(arts.into_iter().cloned());
        }
    }
    out.sort();
    out.dedup();
    out
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
        unique: None,
    }
}

/// poe.ninja lists 5L/6L uniques separately; anything below 5 links is the
/// unlinked line. Of a unique's variants, those the item's mods match (see
/// matching_variants), or all of them when its mods can't tell.
/// `named`: label each price with its unique's name (an unidentified item,
/// which could be any of them).
fn unique_prices(db: &PriceDb, lines: &[ItemLine], item: &Item, named: bool) -> Vec<Price> {
    // PoE2 lists a unique on each of its bases ("Runemastered Wrapped
    // Greathelm" and "Wrapped Greathelm"): the item's own, when listed.
    let same_base: Vec<&ItemLine> = lines.iter().filter(|l| l.base_type.as_deref() == Some(&item.base)).collect();
    let lines: Vec<&ItemLine> = if same_base.is_empty() { lines.iter().collect() } else { same_base };
    let want = if item.links >= 5 { Some(item.links) } else { None };
    let mut matching: Vec<&ItemLine> = lines.iter().copied().filter(|l| l.links.filter(|&n| n >= 5) == want).collect();
    if matching.is_empty() {
        matching = lines;
    }
    let mut matching = matching_variants(matching, item);
    // Most-listed first: those prices are the most trustworthy.
    matching.sort_by_key(|l| std::cmp::Reverse(l.listing_count.unwrap_or(0)));
    matching
        .into_iter()
        .take(MAX_PRICES)
        .map(|l| {
            let mut price = line_price(db, l);
            // Unidentified: several uniques share the base, so say which.
            if named {
                price.unique = Some(l.name.clone());
                price.label =
                    if price.label.is_empty() { l.name.clone() } else { format!("{} ({})", l.name, price.label) };
            }
            price
        })
        .collect()
}

/// A unique's variants (Ralakesh's Impatience's "Power" and "Frenzy",
/// Foulborn uniques' mutations) differ in some of their mods. Each line
/// scores the mods that set it apart from the others: +1 for each the item
/// has, -1 for each it lacks (so a mod poe.ninja words differently from the
/// game costs a point, not the match). The best-scoring lines are kept; all
/// of them when there's nothing to compare (an unidentified item, lines
/// without mods).
fn matching_variants<'a>(lines: Vec<&'a ItemLine>, item: &Item) -> Vec<&'a ItemLine> {
    if lines.len() < 2 || item.mods.is_empty() {
        return lines;
    }
    let have: Vec<(String, Vec<f64>)> = item
        .mods
        .iter()
        .map(|m| {
            let (template, values) = templatize(&m.text);
            (unsigned(&template), values)
        })
        .collect();
    let patterns: Vec<Vec<ModPattern>> = lines.iter().map(|l| l.mod_lines().map(mod_pattern).collect()).collect();
    let mut any_apart = false;
    let scores: Vec<i32> = patterns
        .iter()
        .enumerate()
        .map(|(i, own)| {
            let others: Vec<&Vec<ModPattern>> =
                patterns.iter().enumerate().filter(|&(j, _)| j != i).map(|(_, o)| o).collect();
            own.iter()
                .filter_map(|p| {
                    let found = match p.apart(&others)? {
                        // Only this line has the mod: any roll of it counts
                        // (a line from few listings gives its rolls, not ranges).
                        Apart::ByText => have.iter().any(|(t, _)| *t == p.template),
                        // "Leftmost 3 Magic Utility Flasks" vs 4: the number counts.
                        Apart::ByNumbers => have.iter().any(|(t, v)| p.matches(t, v)),
                    };
                    any_apart = true;
                    Some(if found { 1 } else { -1 })
                })
                .sum()
        })
        .collect();
    if !any_apart {
        return lines;
    }
    let best = scores.iter().copied().max().unwrap_or(0);
    lines.into_iter().zip(scores).filter(|&(_, s)| s == best).map(|(l, _)| l).collect()
}

/// How a mod sets its line apart from the others.
enum Apart {
    /// No other line has this mod.
    ByText,
    /// Others have it, but not every one with these numbers.
    ByNumbers,
}

/// A mod as poe.ninja words it: its template, as unsigned() makes it ("#%
/// to cold resistance"), and for each number the number itself, or None
/// for a roll range ("(15-25)"), which any roll matches.
#[derive(Debug)]
struct ModPattern {
    template: String,
    values: Vec<Option<f64>>,
}

impl ModPattern {
    fn matches(&self, template: &str, values: &[f64]) -> bool {
        self.template == template
            && self.values.len() == values.len()
            && self.values.iter().zip(values).all(|(want, have)| want.is_none_or(|w| (w - have).abs() < 1e-6))
    }

    /// Whether some roll of `self` could also be `other`'s.
    fn overlaps(&self, other: &ModPattern) -> bool {
        self.template == other.template
            && self.values.len() == other.values.len()
            && self.values.iter().zip(&other.values).all(|(a, b)| match (a, b) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-6,
                _ => true,
            })
    }

    /// None when every other line has this mod too.
    fn apart(&self, others: &[&Vec<ModPattern>]) -> Option<Apart> {
        if others.iter().all(|o| o.iter().any(|p| p.overlaps(self))) {
            None
        } else if others.iter().all(|o| o.iter().all(|p| p.template != self.template)) {
            Some(Apart::ByText)
        } else {
            Some(Apart::ByNumbers)
        }
    }
}

fn mod_pattern(text: &str) -> ModPattern {
    let mut template = String::new();
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix('(').and_then(roll_range) {
            template.push('#');
            values.push(None);
            rest = after;
        } else if c.is_ascii_digit() {
            let end = rest.find(|d: char| !d.is_ascii_digit() && d != '.').unwrap_or(rest.len());
            let (num, tail) = rest.split_at(end);
            let (template_part, nums) = templatize(num);
            template.push_str(&template_part);
            values.extend(nums.into_iter().map(Some));
            rest = tail;
        } else {
            template.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    ModPattern { template: unsigned(&template), values }
}

/// A template lowercased and without the signs of its numbers: "+(-13-13)%"
/// rolls "+5%" or "-5%", and "-(50-40)" reads "-44".
fn unsigned(template: &str) -> String {
    let mut out = String::new();
    for c in template.chars() {
        if c == '#' {
            while out.ends_with(['+', '-']) {
                out.pop();
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// "15-25)..." (after the "(") -> what follows the ")". Either end may be
/// negative ("-25--15)").
fn roll_range(s: &str) -> Option<&str> {
    let (inner, after) = s.split_once(')')?;
    let is_num = |n: &str| n.parse::<f64>().is_ok();
    let ok = inner.char_indices().skip(1).any(|(i, c)| c == '-' && is_num(&inner[..i]) && is_num(&inner[i + 1..]));
    ok.then_some(after)
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
/// above the gem's own quality. A level no uncorrupted gem reaches, or
/// quality above 20% (21/20 or 20/23 with "Corrupted" not read), is a
/// corrupted one's.
fn best_gem<'a>(lines: &'a [ItemLine], item: &Item) -> Option<&'a ItemLine> {
    let level = item.gem_level.unwrap_or(1);
    let reaches = |corrupted: bool| {
        lines.iter().any(|l| l.corrupted.unwrap_or(false) == corrupted && l.gem_level.unwrap_or(1) >= level)
    };
    let corrupted = item.corrupted || item.quality > 20 || (!reaches(false) && reaches(true));
    let same_corruption: Vec<&ItemLine> = lines.iter().filter(|l| l.corrupted.unwrap_or(false) == corrupted).collect();
    let pool = if same_corruption.is_empty() { lines.iter().collect() } else { same_corruption };
    let closest = pool.iter().map(|l| l.gem_level.unwrap_or(1).abs_diff(level)).min()?;
    pool.into_iter().filter(|l| l.gem_level.unwrap_or(1).abs_diff(level) == closest).min_by_key(|l| {
        let q = l.gem_quality.unwrap_or(0);
        // Buckets at or below our quality first (closest wins), then above.
        if q <= item.quality { (0, item.quality - q) } else { (1, q - item.quality) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::parse;
    use crate::ninja::tests::{fixture_db, fixture_lines};
    use serde_json::Value;

    /// (label, chaos) of a found price list.
    fn prices(outcome: Outcome) -> Vec<(String, f64)> {
        match outcome {
            Outcome::Found { prices, .. } => prices.into_iter().map(|p| (p.label, p.chaos)).collect(),
            Outcome::NotFound(m) => panic!("not found: {m}"),
            Outcome::Unsupported(m) => panic!("unsupported: {m}"),
        }
    }

    fn look(text: &str) -> Vec<(String, f64)> {
        prices(lookup(&fixture_db(), &parse(text).expect("item text")))
    }

    /// The fixture line of `name` whose `field` is `value` (Null: absent).
    fn line(kind: &str, name: &str, pick: impl Fn(&Value) -> bool) -> Value {
        let lines: Vec<Value> = fixture_lines(kind, name).into_iter().filter(|l| pick(l)).collect();
        assert_eq!(lines.len(), 1, "{name}: want one line, fixture has {lines:?}");
        lines.into_iter().next().unwrap()
    }

    fn chaos(line: &Value) -> f64 {
        line["chaosValue"].as_f64().unwrap()
    }

    fn queen(sockets: &str) -> String {
        format!(
            "Item Class: Body Armours\nRarity: Unique\nQueen of the Forest\nDestiny Leather\n--------\n\
             Evasion Rating: 1200\n--------\nSockets: {sockets}\n--------\nItem Level: 84\n"
        )
    }

    #[test]
    fn experimented_bases() {
        let db = fixture_db();
        let amulet = |ilvl: Option<u32>| Item {
            rarity: "Rare".into(),
            name: Some("Gloom Pendant".into()),
            base: "Simplex Amulet".into(),
            item_level: ilvl,
            ..Default::default()
        };
        // Without an item level: the plain base's lowest one poe.ninja lists.
        let lowest = fixture_lines("BaseType", "Simplex Amulet")
            .into_iter()
            .filter(|l| l["variant"].is_null())
            .min_by_key(|l| l["levelRequired"].as_u64().unwrap())
            .unwrap();
        let p = experimented_price(&db, &amulet(None)).unwrap();
        assert_eq!(p.chaos, chaos(&lowest));
        assert_eq!(p.label, format!("ilvl {}", lowest["levelRequired"]));
        // With one: that item level's, whatever the item's rarity and mods.
        let at85 = line("BaseType", "Simplex Amulet", |l| l["variant"].is_null() && l["levelRequired"] == 85);
        assert_eq!(experimented_price(&db, &amulet(Some(85))).unwrap().chaos, chaos(&at85));
        // Not an experimented base.
        let regalia = Item { base: "Vaal Regalia".into(), ..amulet(None) };
        assert!(experimented_price(&db, &regalia).is_none());
        assert_eq!(EXPERIMENTED_BASES.len(), 40);
    }

    #[test]
    fn unique_by_links() {
        let unlinked = line("UniqueArmour", "Queen of the Forest", |l| l["links"].is_null());
        assert_eq!(look(&queen("G-G-G G-G G")), vec![(String::new(), chaos(&unlinked))]);
        for links in [5, 6] {
            let linked = line("UniqueArmour", "Queen of the Forest", |l| l["links"] == links);
            let sockets = if links == 5 { "G-G-G-G-G G" } else { "G-G-G-G-G-G" };
            assert_eq!(look(&queen(sockets)), vec![(format!("{links}L"), chaos(&linked))], "{links}L");
        }
    }

    /// A fixture line's mods as an item has them: each roll range at its
    /// low end.
    fn rolled_mods(l: &Value) -> Vec<String> {
        let mut out = Vec::new();
        for kind in ["implicitModifiers", "explicitModifiers", "mutatedModifiers"] {
            for m in l[kind].as_array().into_iter().flatten() {
                for text in m["text"].as_str().unwrap().lines() {
                    let (mut line, mut rest) = (String::new(), text);
                    while let Some(i) = rest.find('(') {
                        line.push_str(&rest[..i]);
                        let after = &rest[i + 1..];
                        match roll_range(after) {
                            Some(tail) => {
                                let inner = &after[..after.len() - tail.len() - 1];
                                let k = inner[1..].find('-').unwrap() + 1;
                                line.push_str(&inner[..k]);
                                rest = tail;
                            }
                            None => {
                                line.push('(');
                                rest = after;
                            }
                        }
                    }
                    line.push_str(rest);
                    out.push(line);
                }
            }
        }
        out
    }

    fn unique_text(name: &str, base: &str, sockets: &str, mods: &[String]) -> String {
        format!(
            "Item Class: Body Armours\nRarity: Unique\n{name}\n{base}\n--------\nSockets: {sockets}\n--------\n\
             Item Level: 84\n--------\n{}\n",
            mods.join("\n")
        )
    }

    #[test]
    fn unique_variant_by_mods() {
        // Each Foulborn mutation, and both, at every link count.
        let lines = fixture_lines("UniqueArmour", "Foulborn Queen of the Forest");
        assert_eq!(lines.len(), 9);
        for l in &lines {
            let sockets = match l["links"].as_u64() {
                Some(5) => "G-G-G-G-G G",
                Some(6) => "G-G-G-G-G-G",
                _ => "G-G-G G-G G",
            };
            let text = unique_text("Foulborn Queen of the Forest", "Destiny Leather", sockets, &rolled_mods(l));
            let label = l["variant"].as_str().unwrap();
            let label = match l["links"].as_u64() {
                Some(n) => format!("{label}, {n}L"),
                None => label.to_string(),
            };
            assert_eq!(look(&text), vec![(label, chaos(l))], "{text}");
        }
    }

    #[test]
    fn unique_variant_with_a_missing_mod() {
        // The game no longer shows "-1 to Maximum Power Charges", which
        // poe.ninja still lists: the variant's other mod still picks it.
        let power = line("UniqueArmour", "Ralakesh's Impatience", |l| l["variant"] == "Power");
        let mods: Vec<String> = rolled_mods(&power).into_iter().filter(|m| !m.starts_with("-1 to Maximum")).collect();
        let text = unique_text("Ralakesh's Impatience", "Riveted Boots", "B-B-B-B", &mods);
        assert_eq!(look(&text), vec![("Power".to_string(), chaos(&power))]);

        // No mods to go by: every variant.
        let bare = unique_text("Ralakesh's Impatience", "Riveted Boots", "B-B-B-B", &[]);
        assert_eq!(look(&bare).len(), 3);
    }

    #[test]
    fn mod_patterns() {
        let matches = |ninja: &str, item: &str| {
            let (t, v) = templatize(item);
            mod_pattern(ninja).matches(&unsigned(&t), &v)
        };
        assert!(matches("+(15-25)% to Cold Resistance", "+18% to Cold Resistance"));
        assert!(matches("-(50-40) Physical Damage taken from Hits", "-44 Physical Damage taken from Hits"));
        assert!(matches("(-25--15)% reduced Mana Cost", "-20% reduced Mana Cost"));
        assert!(matches("+(-13-13)% to Chaos Resistance", "-5% to Chaos Resistance"));
        assert!(matches("+(-13-13)% to Chaos Resistance", "+5% to Chaos Resistance"));
        assert!(matches("Adds 50 to 70 Cold Damage to Spells", "Adds 50 to 70 Cold Damage to Spells"));
        assert!(!matches("Leftmost 4 Magic Utility Flasks", "Leftmost 3 Magic Utility Flasks"));
        assert!(!matches("+(15-25)% to Cold Resistance", "+18% to Fire Resistance"));
    }

    #[test]
    fn unidentified_unique_by_art() {
        let item = parse(
            "Item Class: Body Armours\nRarity: Unique\nSpidersilk Robe\n--------\nItem Level: 84\n--------\nUnidentified\n",
        )
        .unwrap();
        let mut db = fixture_db();
        // Without every unique's art, none can be ruled out.
        assert!(unique_arts(&db, &item).is_empty());

        // Foulborn uniques look like their originals.
        for line in db.uniques_by_base.get_mut("Spidersilk Robe").unwrap() {
            let art = line.name.trim_start_matches("Foulborn ").replace(' ', "");
            line.icon = Some(format!("https://web.poecdn.com/{art}.png"));
        }
        let arts: Vec<(String, Vec<String>)> = unique_arts(&db, &item);
        let names = |art: &str| arts.iter().find(|(url, _)| url.ends_with(&format!("/{art}.png"))).unwrap().1.clone();
        assert_eq!(arts.len(), 3);
        assert_eq!(names("SoulMantle"), ["Soul Mantle", "Foulborn Soul Mantle"]);
        assert_eq!(names("ReplicaCovenant"), ["Replica Covenant"]);

        // One unique: priced like an identified one.
        let Outcome::Found { title, prices: got } = unique_prices_named(&db, &item, &names("ReplicaCovenant")) else {
            panic!("not found")
        };
        assert_eq!(title, "Replica Covenant");
        let replica = line("UniqueArmour", "Replica Covenant", |l| l["links"].is_null());
        assert_eq!(got.iter().map(|p| (p.label.as_str(), p.chaos)).collect::<Vec<_>>(), [("", chaos(&replica))]);
        assert_eq!(got[0].unique, None); // nothing left to pick
        // Several: still named.
        let Outcome::Found { title, prices: got } = unique_prices_named(&db, &item, &names("SoulMantle")) else {
            panic!("not found")
        };
        assert_eq!(title, "Soul Mantle / Foulborn Soul Mantle");
        assert_eq!(
            got.iter().map(|p| (p.label.as_str(), p.chaos)).collect::<Vec<_>>(),
            [
                ("Soul Mantle", chaos(&line("UniqueArmour", "Soul Mantle", |l| l["links"].is_null()))),
                (
                    "Foulborn Soul Mantle (Flamewood)",
                    chaos(&line("UniqueArmour", "Foulborn Soul Mantle", |l| l["links"].is_null()))
                )
            ]
        );
        // Each row says which unique it is, to be picked from the panel.
        let picks: Vec<_> = got.iter().map(|p| p.unique.as_deref()).collect();
        assert_eq!(picks, [Some("Soul Mantle"), Some("Foulborn Soul Mantle")]);

        // Identified uniques have nothing to tell apart.
        let named =
            parse("Item Class: Body Armours\nRarity: Unique\nSoul Mantle\nSpidersilk Robe\n--------\nItem Level: 84\n")
                .unwrap();
        assert!(unique_arts(&db, &named).is_empty());
    }

    #[test]
    fn unidentified_unique_by_base() {
        let item = parse(
            "Item Class: Body Armours\nRarity: Unique\nSpidersilk Robe\n--------\nItem Level: 84\n--------\nUnidentified\n",
        )
        .unwrap();
        // poe.ninja happens to send lines most listed first; don't rely on it.
        let mut db = fixture_db();
        db.uniques_by_base.get_mut("Spidersilk Robe").unwrap().reverse();
        let got = prices(lookup(&db, &item));
        // Every unique on the base that isn't 5L/6L, most listed first, by name.
        let mut want: Vec<&ItemLine> =
            db.uniques_by_base["Spidersilk Robe"].iter().filter(|l| l.links.is_none()).collect();
        want.sort_by_key(|l| std::cmp::Reverse(l.listing_count.unwrap_or(0)));
        let want: Vec<(String, f64)> = want
            .iter()
            .take(MAX_PRICES)
            .map(|l| match &l.variant {
                Some(v) => (format!("{} ({v})", l.name), l.chaos_value),
                None => (l.name.clone(), l.chaos_value),
            })
            .collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), MAX_PRICES);
    }

    fn arc(level: &str, quality: Option<u32>, corrupted: bool) -> Vec<(String, f64)> {
        let mut text = format!("Item Class: Skill Gems\nRarity: Gem\nArc\n--------\nLevel: {level}\n");
        if let Some(q) = quality {
            text += &format!("Quality: +{q}% (augmented)\n");
        }
        if corrupted {
            text += "--------\nCorrupted\n";
        }
        look(&text)
    }

    #[test]
    fn gem_variants() {
        let variant = |v: &str| {
            let l = line("SkillGem", "Arc", |l| l["variant"] == v);
            vec![(v.to_string(), chaos(&l))]
        };
        assert_eq!(arc("20 (Max)", Some(20), false), variant("20/20"));
        assert_eq!(arc("20 (Max)", None, false), variant("20"));
        assert_eq!(arc("21 (Max)", Some(20), true), variant("21/20c"));
        // "Corrupted" not read: no uncorrupted Arc is level 21.
        assert_eq!(arc("21 (Max)", Some(20), false), variant("21/20c"));
        // Only corruption takes quality above 20%.
        assert!(fixture_lines("SkillGem", "Arc").iter().all(|l| l["variant"] != "20/23"));
        assert_eq!(arc("20 (Max)", Some(23), false), variant("20/23c"));
        // No 20/22c line: the highest quality not above ours.
        assert_eq!(arc("20 (Max)", Some(22), true), variant("20/20c"));
        // A level poe.ninja doesn't list: the closest one.
        assert_eq!(arc("19", Some(20), false), variant("20/20"));
    }

    #[test]
    fn currency_stack() {
        let got = look("Item Class: Stackable Currency\nRarity: Currency\nDivine Orb\n--------\nStack Size: 3/20\n");
        let each = fixture_db().exchange["Divine Orb"];
        assert_eq!(got, vec![("each".to_string(), each), ("stack of 3".to_string(), each * 3.0)]);
    }

    #[test]
    fn map_by_name() {
        let got = look("Item Class: Maps\nRarity: Normal\nNightmare Map\n--------\nItem Level: 84\n");
        let l = line("Map", "Nightmare Map", |_| true);
        assert_eq!(got, vec![(l["variant"].as_str().unwrap().to_string(), chaos(&l))]);
    }

    fn crown(implicit: &str) -> String {
        format!(
            "Item Class: Viscera\nCoven Queen's Crown\n--------\nUpper Limb\n--------\nItem Level: 84\n--------\n\
             {{ Implicit Modifier }}\n{implicit}\n--------\n\
             Take this to the Wickerman in a town or hideout to attach it.\n"
        )
    }

    #[test]
    fn viscera_by_implicit() {
        let l = line("Viscera", "Coven Queen's Crown", |l| l["variant"] == "#% increased Intelligence");
        assert_eq!(look(&crown("12(10-15)% increased Intelligence")), vec![(String::new(), chaos(&l))]);
        // An implicit poe.ninja has no line for: the most listed ones.
        let got = look(&crown("+1 to Level of all Nonexistent Gems"));
        let mut lines = fixture_lines("Viscera", "Coven Queen's Crown");
        lines.sort_by_key(|l| std::cmp::Reverse(l["listingCount"].as_u64().unwrap_or(0)));
        let want: Vec<(String, f64)> =
            lines.iter().take(MAX_PRICES).map(|l| (l["variant"].as_str().unwrap().to_string(), chaos(l))).collect();
        assert_eq!(got, want);
    }

    fn fixture_chaos(kind: &str, base: &str, pick: impl Fn(&Value) -> bool) -> f64 {
        let json = match kind {
            "BaseType" => include_str!("../fixtures/ninja-base-type.json"),
            _ => include_str!("../fixtures/ninja-cluster-jewel.json"),
        };
        let v: Value = serde_json::from_str(json).unwrap();
        let line = v["lines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| (l["baseType"] == base || l["name"] == base) && pick(l))
            .unwrap()
            .clone();
        chaos(&line)
    }

    fn regalia(rarity: &str, ilvl: u32, extra: &str) -> Vec<(String, f64)> {
        let text = format!(
            "Item Class: Body Armours\nRarity: {rarity}\nVaal Regalia\n--------\nItem Level: {ilvl}\n--------\n{extra}"
        );
        base_prices(&fixture_db(), &parse(&text).unwrap()).into_iter().map(|p| (p.label, p.chaos)).collect()
    }

    #[test]
    fn poe2_unique_by_base() {
        let db = crate::ninja::tests::poe2_fixture_db();
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../fixtures/poe2/ninja-unique-armours.json")).unwrap();
        let per_divine = raw["core"]["rates"]["exalted"].as_f64().unwrap();
        let line = |base: &str| {
            raw["lines"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["name"] == "Black Sun Crest" && l["baseType"] == base)
                .cloned()
        };
        for base in ["Wrapped Greathelm", "Runemastered Wrapped Greathelm"] {
            let Some(want) = line(base) else { continue };
            let text =
                format!("Item Class: Helmets\nRarity: Unique\nBlack Sun Crest\n{base}\n--------\nItem Level: 80\n");
            let Outcome::Found { prices, .. } = lookup(&db, &crate::item::parse(&text).unwrap()) else {
                panic!("no price for Black Sun Crest on {base}")
            };
            let divines = want["primaryValue"].as_f64().unwrap();
            assert_eq!(prices.len(), 1, "{base}: {prices:?}");
            assert!((prices[0].divine - divines).abs() < 1e-9, "{base}: {prices:?}");
            assert!((prices[0].chaos - divines * per_divine).abs() < 1e-6, "in exalted: {prices:?}");
        }
    }

    #[test]
    fn base_type_by_level_and_influence() {
        let plain = fixture_chaos("BaseType", "Vaal Regalia", |l| l["levelRequired"] == 86 && l["variant"].is_null());
        assert_eq!(regalia("Normal", 87, ""), vec![("ilvl 86+".to_string(), plain)]);
        let hunter =
            fixture_chaos("BaseType", "Vaal Regalia", |l| l["levelRequired"] == 85 && l["variant"] == "Hunter");
        assert_eq!(regalia("Normal", 85, "Hunter Item\n"), vec![("ilvl 85 Hunter".to_string(), hunter)]);
        // Not a plain base: no base price.
        assert!(regalia("Normal", 85, "Corrupted\n").is_empty());
        assert!(regalia("Magic", 85, "").is_empty());
        assert!(regalia("Normal", 81, "").is_empty());
    }

    #[test]
    fn cluster_by_enchant_passives_and_bracket() {
        let jewel = |ilvl: u32, passives: u32| {
            let text = format!(
                "Item Class: Jewels\nRarity: Normal\nLarge Cluster Jewel\n--------\nItem Level: {ilvl}\n--------\n\
                 Adds {passives} Passive Skills (enchant)\n2 Added Passive Skills are Jewel Sockets (enchant)\n\
                 Added Small Passive Skills grant: 10% increased Spell Damage (enchant)\n"
            );
            base_prices(&fixture_db(), &parse(&text).unwrap())
                .into_iter()
                .map(|p| (p.label, p.chaos))
                .collect::<Vec<_>>()
        };
        let want = fixture_chaos("ClusterJewel", "Large Cluster Jewel", |l| {
            l["name"] == "10% increased Spell Damage" && l["variant"] == "8 passives" && l["levelRequired"] == 75
        });
        // Item level 80 sells in the 75+ bracket.
        assert_eq!(jewel(80, 8), vec![("ilvl 75+".to_string(), want)]);
        assert!(jewel(80, 7).is_empty());
    }

    #[test]
    fn not_priced() {
        let db = fixture_db();
        let rare = parse("Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n+60 to maximum Life\n").unwrap();
        assert!(matches!(lookup(&db, &rare), Outcome::Unsupported(_)));
        let unknown = parse(
            "Item Class: Body Armours\nRarity: Unique\nNo Such Unique\nDestiny Leather\n--------\nItem Level: 84\n",
        )
        .unwrap();
        assert!(matches!(lookup(&db, &unknown), Outcome::NotFound(m) if m.contains("No Such Unique")));
    }
}
