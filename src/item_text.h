// Parses the text PoE puts on the clipboard for Ctrl+C on an item:
//
//   Item Class: Body Armours
//   Rarity: Rare
//   Dusk Shell            <- name (absent for normal/magic: one line only)
//   Vaal Regalia          <- base type
//   --------
//   ...more sections...
//
// Only the header is interpreted for now; the rest is kept as raw sections.
#pragma once

#include <optional>
#include <string>
#include <string_view>
#include <vector>

struct ParsedItem {
    std::string item_class;
    std::string rarity;
    std::string name;  // empty when the item has only a base type line
    std::string base;
    std::vector<std::vector<std::string>> sections;  // after the header
};

// nullopt if the text isn't PoE item text.
std::optional<ParsedItem> parse_item_text(std::string_view text);
