//! Parser for the text PoE1 puts on the clipboard for Ctrl+C on an item,
//! and for Ctrl+Alt+C ("advanced" copy: mod headers and roll ranges).

#[derive(Debug, Default)]
pub struct Item {
    pub class: String,
    pub rarity: String,
    /// Unique/rare name; `None` for items with only a base type line
    /// (normal/magic items, currency, gems, unidentified uniques).
    pub name: Option<String>,
    pub base: String,
    pub corrupted: bool,
    pub gem_level: Option<u32>,
    pub quality: u32,
    /// Size of the largest linked socket group.
    pub links: u32,
    pub map_tier: Option<u32>,
    pub stack_size: Option<u32>,
    pub item_level: Option<u32>,
    /// Lines after the "Item Level:" section: enchants, implicits, explicits,
    /// plus some non-mod lines that simply won't match any trade stat.
    /// Roll ranges from advanced copy are stripped.
    pub mods: Vec<ModLine>,
    /// Advanced copy marks each mod with a `{ … Modifier … }` header, so
    /// mods[i].headed tells real mods from other text.
    pub advanced: bool,
    /// Map-ish properties in display order, e.g. ("Item Quantity", "+85%").
    pub map_props: Vec<(String, String)>,
    /// Weapon properties: average damage per hit by type, attacks per second.
    pub physical_damage: Option<f64>,
    pub elemental_damage: f64,
    pub attacks_per_second: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModKind {
    Explicit,
    Implicit,
    Crafted,
    Fractured,
    Enchant,
}

impl ModKind {
    /// The trade API's stat group id.
    pub fn trade_group(self) -> &'static str {
        match self {
            ModKind::Explicit => "explicit",
            ModKind::Implicit => "implicit",
            ModKind::Crafted => "crafted",
            ModKind::Fractured => "fractured",
            ModKind::Enchant => "enchant",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModLine {
    pub kind: ModKind,
    /// The mod text without its "(implicit)"-style marker.
    pub text: String,
    /// Came after a `{ … Modifier … }` header (advanced copy only).
    pub headed: bool,
    /// For each number in `text`, in order: its tier's roll range, if the
    /// text came from advanced copy.
    pub ranges: Vec<Option<(f64, f64)>>,
}

fn mod_line(line: &str) -> ModLine {
    const MARKERS: &[(&str, ModKind)] = &[
        (" (implicit)", ModKind::Implicit),
        (" (crafted)", ModKind::Crafted),
        (" (fractured)", ModKind::Fractured),
        (" (enchant)", ModKind::Enchant),
    ];
    for (marker, kind) in MARKERS {
        if let Some(text) = line.strip_suffix(marker) {
            return ModLine { kind: *kind, text: text.to_string(), headed: false, ranges: Vec::new() };
        }
    }
    ModLine { kind: ModKind::Explicit, text: line.to_string(), headed: false, ranges: Vec::new() }
}

/// Map properties shown in the map check, in this order.
const MAP_PROPS: &[&str] = &[
    "Item Quantity", "Item Rarity", "Monster Pack Size", "More Currency", "More Scarabs", "More Maps",
    "More Divination Cards", "Quality",
];

/// Advanced copy appends the roll range to each number: "+60(55-69)" and
/// "+25(20-30)%" become "+60" and "+25%". Other parentheses are kept.
fn strip_roll_ranges(line: &str) -> String {
    split_roll_ranges(line).0
}

/// The text without roll ranges, and for each number in it, in order, the
/// range that followed it (as a positive range for "-12(-12--9)").
fn split_roll_ranges(line: &str) -> (String, Vec<Option<(f64, f64)>>) {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len()
            && (chars[i].is_ascii_digit() || (chars[i] == '.' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())))
        {
            i += 1;
        }
        out.extend(&chars[start..i]);
        let negative = start > 0 && chars[start - 1] == '-';
        let mut range = None;
        if chars.get(i) == Some(&'(') {
            if let Some(len) = chars[i + 1..].iter().position(|&c| c == ')') {
                let inner: String = chars[i + 1..i + 1 + len].iter().collect();
                if let Some((a, b)) = parse_range(&inner) {
                    range = Some(if negative {
                        let (a, b) = (a.abs(), b.abs());
                        (a.min(b), a.max(b))
                    } else {
                        (a, b)
                    });
                    i += len + 2;
                }
            }
        }
        ranges.push(range);
    }
    (out, ranges)
}

/// "55-69", "0.6-1", "-12--9", or a fixed "18".
fn parse_range(inner: &str) -> Option<(f64, f64)> {
    if !inner.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') || !inner.chars().any(|c| c.is_ascii_digit()) {
        return None;
    }
    let bytes = inner.as_bytes();
    for k in 1..bytes.len() {
        if bytes[k] == b'-' && bytes[k - 1].is_ascii_digit() {
            let (a, b): (f64, f64) = (inner[..k].parse().ok()?, inner[k + 1..].parse().ok()?);
            return Some((a.min(b), a.max(b)));
        }
    }
    let v: f64 = inner.parse().ok()?;
    Some((v, v))
}

/// "45-120 (augmented)" or "10-20 (augmented), 5-50 (augmented)": the sum of
/// each range's average.
fn average_damage(value: &str) -> f64 {
    value
        .split(", ")
        .filter_map(|part| {
            let part = part.trim_end_matches(" (augmented)");
            let (a, b) = part.split_once('-')?;
            Some((a.trim().parse::<f64>().ok()? + b.trim().parse::<f64>().ok()?) / 2.0)
        })
        .sum()
}

pub fn parse(text: &str) -> Option<Item> {
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line == "--------" {
            sections.push(Vec::new());
        } else if !line.is_empty() {
            sections.last_mut().unwrap().push(line);
        }
    }

    let mut item = Item::default();
    let mut names = Vec::new();
    for line in &sections[0] {
        if let Some(v) = line.strip_prefix("Item Class: ") {
            item.class = v.to_string();
        } else if let Some(v) = line.strip_prefix("Rarity: ") {
            item.rarity = v.to_string();
        } else {
            names.push(*line);
        }
    }
    match names.as_slice() {
        [base] => item.base = base.to_string(),
        [name, base] => {
            item.name = Some(name.to_string());
            item.base = base.to_string();
        }
        _ => return None,
    }
    // Some items have no rarity (Viscera limbs), but every item has a class.
    if item.rarity.is_empty() && item.class.is_empty() {
        return None;
    }

    let mut after_item_level = false;
    for section in sections.iter().skip(1) {
        if after_item_level {
            // In advanced copy every mod has a "{ … }" header, so a later
            // section without one is flavour text or a note ("Take this to
            // the Wickerman…") — unless its lines carry a kind marker like
            // "(enchant)", which some mods show instead.
            let has_header = section.iter().any(|l| l.starts_with("{ ") && l.ends_with(" }"));
            if item.advanced && !has_header && section.iter().all(|l| mod_line(l).kind == ModKind::Explicit) {
                continue;
            }
            let mut headed = false;
            for line in section {
                if line.starts_with("{ ") && line.ends_with(" }") {
                    item.advanced = true;
                    headed = true;
                    continue;
                }
                // Advanced copy's reminder text, e.g. "(Ignited enemies take…)".
                if item.advanced && line.starts_with('(') && line.ends_with(')') {
                    continue;
                }
                // Advanced copy notes mods that quality doesn't scale.
                let line = line.trim_end_matches(" — Unscalable Value");
                let (text, ranges) = split_roll_ranges(line);
                let mut m = mod_line(&text);
                m.headed = headed;
                m.ranges = ranges;
                item.mods.push(m);
            }
        } else if let Some(v) = section.iter().find_map(|l| l.strip_prefix("Item Level: ")) {
            item.item_level = leading_number(v);
            after_item_level = true;
        }
    }

    for line in sections.iter().skip(1).flatten() {
        if *line == "Corrupted" {
            item.corrupted = true;
        } else if let Some(v) = line.strip_prefix("Level: ") {
            // Only gems have a bare "Level:" line ("Level: 20 (Max)").
            if item.rarity == "Gem" {
                item.gem_level = leading_number(v);
            }
        } else if let Some(v) = line.strip_prefix("Quality: ") {
            item.quality = leading_number(v).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Sockets: ") {
            item.links = largest_link_group(v);
        } else if let Some(v) = line.strip_prefix("Map Tier: ") {
            item.map_tier = leading_number(v);
        } else if let Some(v) = line.strip_prefix("Stack Size: ") {
            item.stack_size = leading_number(v);
        } else if let Some(v) = line.strip_prefix("Physical Damage: ") {
            item.physical_damage = Some(average_damage(v));
        } else if let Some(v) = line.strip_prefix("Elemental Damage: ") {
            item.elemental_damage += average_damage(v);
        } else if let Some(v) = line.strip_prefix("Attacks per Second: ") {
            item.attacks_per_second = v.trim_end_matches(" (augmented)").parse().ok();
        }
        if let Some((label, value)) = line.split_once(": ") {
            if MAP_PROPS.contains(&label) {
                let value = strip_roll_ranges(value.trim_end_matches(" (augmented)"));
                item.map_props.push((label.to_string(), value));
            }
        }
    }
    Some(item)
}

/// First run of digits, ignoring a leading '+' and thousands separators:
/// "+20% (augmented)" -> 20, "1,234/5,000" -> 1234, "20 (Max)" -> 20.
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s
        .trim_start_matches('+')
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    digits.parse().ok()
}

/// "R-R-G B-B" -> 3: groups are space-separated, links are dashes.
fn largest_link_group(sockets: &str) -> u32 {
    sockets
        .split_whitespace()
        .map(|group| group.matches('-').count() as u32 + 1)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_with_links() {
        let text = "Item Class: Body Armours\r\nRarity: Unique\r\nKaom's Heart\r\nGlorious Plate\r\n\
                    --------\r\nArmour: 553\r\n--------\r\nSockets: R-R-R-R-R-R \r\n--------\r\nItem Level: 85\r\n";
        let item = parse(text).unwrap();
        assert_eq!(item.name.as_deref(), Some("Kaom's Heart"));
        assert_eq!(item.base, "Glorious Plate");
        assert_eq!(item.links, 6);
    }

    #[test]
    fn gem() {
        let text = "Item Class: Skill Gems\nRarity: Gem\nDischarge\n--------\nLevel: 20 (Max)\n\
                    Quality: +20% (augmented)\n--------\nCorrupted\n";
        let item = parse(text).unwrap();
        assert_eq!(item.base, "Discharge");
        assert_eq!(item.gem_level, Some(20));
        assert_eq!(item.quality, 20);
        assert!(item.corrupted);
    }

    #[test]
    fn currency_stack() {
        let text = "Item Class: Stackable Currency\nRarity: Currency\nChaos Orb\n--------\nStack Size: 1,234/5,000\n";
        let item = parse(text).unwrap();
        assert_eq!(item.base, "Chaos Orb");
        assert_eq!(item.stack_size, Some(1234));
    }

    #[test]
    fn rare_mods() {
        let text = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\n\
                    Requirements:\nLevel: 40\n--------\nItem Level: 84\n--------\n\
                    +25% to Cold Resistance (implicit)\n--------\n+60 to maximum Life\n\
                    +30% to Fire Resistance\n+12% to Cold Resistance (crafted)\n--------\nCorrupted\n";
        let item = parse(text).unwrap();
        assert_eq!(item.item_level, Some(84));
        let mods: Vec<_> = item.mods.iter().map(|m| (m.kind, m.text.as_str())).collect();
        assert_eq!(
            mods,
            vec![
                (ModKind::Implicit, "+25% to Cold Resistance"),
                (ModKind::Explicit, "+60 to maximum Life"),
                (ModKind::Explicit, "+30% to Fire Resistance"),
                (ModKind::Crafted, "+12% to Cold Resistance"),
                (ModKind::Explicit, "Corrupted"),
            ]
        );
    }

    #[test]
    fn advanced_copy() {
        let text = "Item Class: Rings\nRarity: Rare\nDoom Loop\nSapphire Ring\n--------\nItem Level: 84\n--------\n\
                    { Implicit Modifier — Elemental, Cold, Resistance }\n+25(20-30)% to Cold Resistance (implicit)\n--------\n\
                    { Prefix Modifier \"Virile\" (Tier: 3) — Life }\n+60(55-69) to maximum Life\n\
                    { Suffix Modifier \"of the Furnace\" (Tier: 4) — Elemental, Fire, Resistance }\n\
                    +30(30-35)% to Fire Resistance\n(Reminder text)\n";
        let item = parse(text).unwrap();
        assert!(item.advanced);
        let mods: Vec<_> = item.mods.iter().map(|m| (m.kind, m.text.as_str(), m.headed)).collect();
        assert_eq!(
            mods,
            vec![
                (ModKind::Implicit, "+25% to Cold Resistance", true),
                (ModKind::Explicit, "+60 to maximum Life", true),
                (ModKind::Explicit, "+30% to Fire Resistance", true),
            ]
        );
    }

    #[test]
    fn unscalable_note() {
        let text = "Item Class: Maps\nRarity: Rare\nTorture Hustle\nMap (Tier 16)\n--------\nItem Level: 83\n--------\n\
                    { Prefix Modifier \"Elder\" }\nArea is influenced by The Elder — Unscalable Value\n";
        let item = parse(text).unwrap();
        assert_eq!(item.mods[0].text, "Area is influenced by The Elder");
    }

    #[test]
    fn range_values() {
        let (text, ranges) = split_roll_ranges("Adds 5(3-6) to 10(9-11) Damage");
        assert_eq!(text, "Adds 5 to 10 Damage");
        assert_eq!(ranges, vec![Some((3.0, 6.0)), Some((9.0, 11.0))]);
        let (_, ranges) = split_roll_ranges("Players have -12(-12--9)% to all maximum Resistances");
        assert_eq!(ranges, vec![Some((9.0, 12.0))]);
        let (_, ranges) = split_roll_ranges("+1 to Level of Socketed Gems");
        assert_eq!(ranges, vec![None]);
    }

    #[test]
    fn weapon_props() {
        let text = "Item Class: Warstaves\nRarity: Rare\nBramble Mast\nEzomyte Staff\n--------\nWarstaff\n\
                    Physical Damage: 100-200 (augmented)\nElemental Damage: 10-20 (augmented), 5-15 (augmented)\n\
                    Attacks per Second: 1.30 (augmented)\n--------\nItem Level: 68\n";
        let item = parse(text).unwrap();
        assert_eq!(item.physical_damage, Some(150.0));
        assert_eq!(item.elemental_damage, 25.0);
        assert_eq!(item.attacks_per_second, Some(1.3));
    }

    #[test]
    fn ranges() {
        assert_eq!(strip_roll_ranges("Adds 5(3-6) to 10(9-11) Damage"), "Adds 5 to 10 Damage");
        assert_eq!(strip_roll_ranges("+0.8(0.6-1)% Leech"), "+0.8% Leech");
        assert_eq!(strip_roll_ranges("Grants Level 20 Aspect (Unique)"), "Grants Level 20 Aspect (Unique)");
        assert_eq!(strip_roll_ranges("Monsters reflect 18(15-20)% of Physical Damage"), "Monsters reflect 18% of Physical Damage");
    }

    #[test]
    fn split_links() {
        assert_eq!(largest_link_group("R-G B-B-B G"), 3);
    }

    #[test]
    fn not_an_item() {
        assert!(parse("hello\nworld").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn viscera_without_rarity() {
        let text = "Item Class: Viscera\nLesser Brute's Arm\n--------\nUpper Limb\n--------\nItem Level: 29\n--------\n\
                    { Implicit Modifier }\n+15(10-15) to Strength\n--------\n\
                    Take this to the Wickerman in a town or hideout to attach it.\n";
        let item = parse(text).unwrap();
        assert_eq!(item.class, "Viscera");
        assert_eq!(item.base, "Lesser Brute's Arm");
        let mods: Vec<&str> = item.mods.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(mods, vec!["+15 to Strength"]);
    }
}
