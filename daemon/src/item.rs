//! Parser for the text PoE1 puts on the clipboard when an item is copied:
//! plain, or the "advanced" copy with mod headers and roll ranges (what
//! Ctrl+C gives now, like Ctrl+Alt+C).

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
    /// "Level: 20 (Max)": the gem can't level further.
    pub gem_max_level: bool,
    pub quality: u32,
    /// Size of the largest linked socket group.
    pub links: u32,
    /// Number of sockets.
    pub sockets: u32,
    pub map_tier: Option<u32>,
    pub stack_size: Option<u32>,
    pub item_level: Option<u32>,
    /// "Memory Strands: 72" on weapons, armour and jewellery.
    pub memory_strands: Option<u32>,
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
    /// "Critical Strike Chance: 6.50%", in percent.
    pub critical_chance: Option<f64>,
    /// "Fractured Item", "Synthesised Item", "Mirrored" lines.
    pub fractured_item: bool,
    pub synthesised: bool,
    pub mirrored: bool,
    /// Influences from "Shaper Item"-style lines, as trade keys ("shaper").
    pub influences: Vec<&'static str>,
    /// "Searing Exarch Item" / "Eater of Worlds Item": an eldritch implicit,
    /// with its tier (Lesser 1 … Perfect 6) when advanced copy names it.
    pub searing: Option<Option<u32>>,
    pub tangled: Option<Option<u32>>,
    /// Armour properties as shown (with the item's quality).
    pub armour: Option<f64>,
    pub evasion: Option<f64>,
    pub energy_shield: Option<f64>,
    pub ward: Option<f64>,
    /// "Chance to Block: 26%" on shields, in percent.
    pub block: Option<f64>,
}

/// Influence lines and their trade site names.
const INFLUENCES: &[(&str, &str)] = &[
    ("Shaper Item", "shaper"),
    ("Elder Item", "elder"),
    ("Crusader Item", "crusader"),
    ("Redeemer Item", "redeemer"),
    ("Hunter Item", "hunter"),
    ("Warlord Item", "warlord"),
];

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

/// Which eldritch altar an implicit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eldritch {
    /// The Searing Exarch.
    Exarch,
    /// The Eater of Worlds.
    Eater,
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
    /// The affix tier from its advanced-copy header ("(Tier: 3)"); 1 is best.
    pub tier: Option<u32>,
    /// An eldritch implicit's altar, from its advanced-copy header.
    pub eldritch: Option<Eldritch>,
}

/// `{ Prefix Modifier "Virile" (Tier: 3) — Life }` -> 3. Crafted mods have
/// a "(Rank: n)" instead, which isn't a tier.
fn header_tier(header: &str) -> Option<u32> {
    let rest = &header[header.find("(Tier: ")? + "(Tier: ".len()..];
    rest[..rest.find(')')?].trim().parse().ok()
}

/// `{ Searing Exarch Implicit Modifier (Grand) — … }` -> (Exarch, Some(3)).
/// The tier is None if the header names one we don't know.
fn eldritch_header(header: &str) -> Option<(Eldritch, Option<u32>)> {
    let eldritch = if header.starts_with("{ Searing Exarch Implicit Modifier") {
        Eldritch::Exarch
    } else if header.starts_with("{ Eater of Worlds Implicit Modifier") {
        Eldritch::Eater
    } else {
        return None;
    };
    let tiers = ["Lesser", "Greater", "Grand", "Exceptional", "Exquisite", "Perfect"];
    let tier = tiers.iter().position(|t| header.contains(&format!("({t})"))).map(|t| t as u32 + 1);
    Some((eldritch, tier))
}

/// Advanced copy marks some mods' kind in their header only: fractured and
/// crafted ones (`{ Fractured Prefix Modifier "Fecund" (Tier: 2) — Life }`),
/// and implicits, eldritch ones included (`{ Searing Exarch Implicit
/// Modifier (Lesser) }`), not with a "(fractured)", "(crafted)" or
/// "(implicit)" after the mod as plain Ctrl+C does.
fn kind_from_header(header: &str) -> Option<ModKind> {
    if header.starts_with("{ Fractured ") {
        Some(ModKind::Fractured)
    } else if header.starts_with("{ Master Crafted ") {
        Some(ModKind::Crafted)
    } else if header.contains("Implicit Modifier") {
        Some(ModKind::Implicit)
    } else {
        None
    }
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
            return ModLine {
                kind: *kind,
                text: text.to_string(),
                headed: false,
                ranges: Vec::new(),
                tier: None,
                eldritch: None,
            };
        }
    }
    ModLine {
        kind: ModKind::Explicit,
        text: line.to_string(),
        headed: false,
        ranges: Vec::new(),
        tier: None,
        eldritch: None,
    }
}

/// Map properties shown in the map check, in this order.
const MAP_PROPS: &[&str] = &[
    "Item Quantity",
    "Item Rarity",
    "Monster Pack Size",
    "More Currency",
    "More Scarabs",
    "More Maps",
    "More Divination Cards",
    "Quality",
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
        if chars.get(i) == Some(&'(')
            && let Some(len) = chars[i + 1..].iter().position(|&c| c == ')')
        {
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
        ranges.push(range);
    }
    (out, ranges)
}

/// "55-69", "0.6-1", "-12--9", or a fixed "18".
fn parse_range(inner: &str) -> Option<(f64, f64)> {
    if !inner.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') || !inner.chars().any(|c| c.is_ascii_digit())
    {
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
    // Magic items' affix names from advanced copy, to find their base.
    let mut affixes: Vec<(bool, String)> = Vec::new();
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
            let mut tier = None;
            let mut header_kind = None;
            let mut eldritch = None;
            for line in section {
                // A property, wherever the game puts it.
                if line.starts_with("Memory Strands: ")
                    || *line == "Searing Exarch Item"
                    || *line == "Eater of Worlds Item"
                {
                    continue;
                }
                if line.starts_with("{ ") && line.ends_with(" }") {
                    item.advanced = true;
                    headed = true;
                    tier = header_tier(line);
                    header_kind = kind_from_header(line);
                    let header = eldritch_header(line);
                    eldritch = header.map(|(e, _)| e);
                    match header {
                        Some((Eldritch::Exarch, t)) => item.searing = Some(t),
                        Some((Eldritch::Eater, t)) => item.tangled = Some(t),
                        None => {}
                    }
                    if let Some(affix) = header_affix(line) {
                        affixes.push(affix);
                    }
                    continue;
                }
                // Advanced copy's reminder text, e.g. "(Ignited enemies take…)".
                if item.advanced && line.starts_with('(') && line.ends_with(')') {
                    continue;
                }
                // Advanced copy notes mods that quality doesn't scale.
                let line = line.trim_end_matches(" — Unscalable Value");
                let (text, mut ranges) = split_roll_ranges(line);
                let mut m = mod_line(&text);
                // Under a header every number that rolls shows its range, so
                // one without is fixed ("target 1 additional nearby Enemy").
                if headed {
                    let (_, values) = crate::trade::templatize(&text);
                    if values.len() == ranges.len() {
                        for (range, v) in ranges.iter_mut().zip(values) {
                            range.get_or_insert((v, v));
                        }
                    }
                }
                if m.kind == ModKind::Explicit
                    && let Some(kind) = header_kind
                {
                    m.kind = kind;
                }
                m.headed = headed;
                m.ranges = ranges;
                m.tier = tier;
                m.eldritch = eldritch;
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
        } else if *line == "Fractured Item" {
            item.fractured_item = true;
        } else if *line == "Synthesised Item" {
            item.synthesised = true;
        } else if *line == "Mirrored" {
            item.mirrored = true;
        } else if *line == "Searing Exarch Item" {
            item.searing.get_or_insert(None);
        } else if *line == "Eater of Worlds Item" {
            item.tangled.get_or_insert(None);
        } else if let Some(&(_, key)) = INFLUENCES.iter().find(|(text, _)| text == line) {
            item.influences.push(key);
        } else if let Some(v) = line.strip_prefix("Level: ") {
            // Only gems have a bare "Level:" line ("Level: 20 (Max)").
            if item.rarity == "Gem" {
                item.gem_level = leading_number(v);
                item.gem_max_level = v.contains("(Max)");
            }
        } else if let Some(v) = line.strip_prefix("Quality: ") {
            item.quality = leading_number(v).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Sockets: ") {
            item.links = largest_link_group(v);
            item.sockets = v.split(|c: char| c == '-' || c.is_whitespace()).filter(|s| !s.is_empty()).count() as u32;
        } else if let Some(v) = line.strip_prefix("Map Tier: ") {
            item.map_tier = leading_number(v);
        } else if let Some(v) = line.strip_prefix("Memory Strands: ") {
            item.memory_strands = leading_number(v);
        } else if let Some(v) = line.strip_prefix("Stack Size: ") {
            item.stack_size = leading_number(v);
        } else if let Some(v) = line.strip_prefix("Physical Damage: ") {
            item.physical_damage = Some(average_damage(v));
        } else if let Some(v) = line.strip_prefix("Elemental Damage: ") {
            item.elemental_damage += average_damage(v);
        } else if let Some(v) = line.strip_prefix("Attacks per Second: ") {
            item.attacks_per_second = v.trim_end_matches(" (augmented)").parse().ok();
        } else if let Some(v) = line.strip_prefix("Armour: ") {
            item.armour = property_number(v);
        } else if let Some(v) = line.strip_prefix("Evasion Rating: ") {
            item.evasion = property_number(v);
        } else if let Some(v) = line.strip_prefix("Energy Shield: ") {
            item.energy_shield = property_number(v);
        } else if let Some(v) = line.strip_prefix("Ward: ") {
            item.ward = property_number(v);
        } else if let Some(v) = line.strip_prefix("Chance to Block: ") {
            item.block = property_number(v);
        } else if let Some(v) = line.strip_prefix("Critical Strike Chance: ") {
            item.critical_chance = v.trim_end_matches(" (augmented)").trim_end_matches('%').parse().ok();
        }
        if let Some((label, value)) = line.split_once(": ")
            && MAP_PROPS.contains(&label)
        {
            let value = strip_roll_ranges(value.trim_end_matches(" (augmented)"));
            item.map_props.push((label.to_string(), value));
        }
    }
    // A magic item's one name line is its base with the affixes around it
    // ("Fecund Paladin Crown of the Genius"); advanced copy names them.
    if item.rarity == "Magic" && item.name.is_none() && !affixes.is_empty() {
        let mut base = item.base.as_str();
        for (prefix, affix) in &affixes {
            base = if *prefix {
                base.strip_prefix(affix.as_str()).and_then(|b| b.strip_prefix(' ')).unwrap_or(base)
            } else {
                base.strip_suffix(affix.as_str()).and_then(|b| b.strip_suffix(' ')).unwrap_or(base)
            };
        }
        let base = base.strip_prefix("Synthesised ").unwrap_or(base).to_string();
        if base != item.base {
            item.name = Some(std::mem::replace(&mut item.base, base));
        }
    }
    Some(item)
}

/// `{ Prefix Modifier "Fecund" (Tier: 2) — Life }` -> (true, "Fecund");
/// suffixes give false. Unnamed (implicit, unique) headers give None.
fn header_affix(header: &str) -> Option<(bool, String)> {
    let prefix = header.contains("Prefix Modifier \"");
    if !prefix && !header.contains("Suffix Modifier \"") {
        return None;
    }
    let start = header.find('"')? + 1;
    let len = header[start..].find('"')?;
    Some((prefix, header[start..start + len].to_string()))
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
/// "553 (augmented)", "26%" -> the number.
fn property_number(v: &str) -> Option<f64> {
    v.split_whitespace().next()?.trim_end_matches('%').parse().ok()
}

fn largest_link_group(sockets: &str) -> u32 {
    sockets.split_whitespace().map(|group| group.matches('-').count() as u32 + 1).max().unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
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
        let tiers: Vec<_> = item.mods.iter().map(|m| m.tier).collect();
        assert_eq!(tiers, vec![None, Some(3), Some(4)]);
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
        assert_eq!(
            strip_roll_ranges("Monsters reflect 18(15-20)% of Physical Damage"),
            "Monsters reflect 18% of Physical Damage"
        );
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

    pub(crate) const FRACTURED_CROWN: &str = "Item Class: Helmets\nRarity: Magic\nFecund Paladin Crown of the Genius\n--------\n\
        Armour: 350\nEnergy Shield: 70\n--------\nItem Level: 84\n--------\n\
        { Fractured Prefix Modifier \"Fecund\" (Tier: 2) — Life }\n+143(130-144) to maximum Life\n\
        { Suffix Modifier \"of the Genius\" (Tier: 2) — Attribute }\n+52(51-55) to Intelligence\n--------\n\
        Fractured Item\n";

    #[test]
    fn magic_base_and_markers() {
        let item = parse(FRACTURED_CROWN).unwrap();
        assert_eq!(item.base, "Paladin Crown");
        assert_eq!(item.name.as_deref(), Some("Fecund Paladin Crown of the Genius"));
        assert!(item.fractured_item);
        let mods: Vec<_> = item.mods.iter().map(|m| (m.kind, m.text.as_str(), m.tier)).collect();
        assert_eq!(
            mods,
            vec![
                (ModKind::Fractured, "+143 to maximum Life", Some(2)),
                (ModKind::Explicit, "+52 to Intelligence", Some(2)),
            ]
        );
        // Plain Ctrl+C marks the mod after it instead.
        let plain = parse("Item Class: Helmets\nRarity: Magic\nFecund Paladin Crown of the Genius\n--------\n\
            Item Level: 84\n--------\n+143 to maximum Life (fractured)\n+52 to Intelligence\n--------\nFractured Item\n")
        .unwrap();
        assert_eq!(plain.mods[0].kind, ModKind::Fractured);
        let crafted = parse(&FRACTURED_CROWN.replace(
            "{ Suffix Modifier \"of the Genius\" (Tier: 2)",
            "{ Master Crafted Suffix Modifier \"of the Genius\" (Rank: 2)",
        ))
        .unwrap();
        assert_eq!(crafted.mods[1].kind, ModKind::Crafted);
        let shaped = parse(&FRACTURED_CROWN.replace("Fractured Item", "Shaper Item\nElder Item")).unwrap();
        assert_eq!(shaped.influences, vec!["shaper", "elder"]);
    }
}
