//! Parser for the text PoE1 puts on the clipboard for Ctrl+C on an item.

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
    if item.rarity.is_empty() {
        return None;
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
    fn split_links() {
        assert_eq!(largest_link_group("R-G B-B-B G"), 3);
    }

    #[test]
    fn not_an_item() {
        assert!(parse("hello\nworld").is_none());
        assert!(parse("").is_none());
    }
}
