#include "item_text.h"

namespace {

bool take_prefix(std::string_view line, std::string_view prefix, std::string* out) {
    if (line.substr(0, prefix.size()) != prefix) return false;
    *out = line.substr(prefix.size());
    return true;
}

}  // namespace

std::optional<ParsedItem> parse_item_text(std::string_view text) {
    std::vector<std::vector<std::string>> sections(1);
    while (!text.empty()) {
        size_t nl = text.find('\n');
        std::string_view line = text.substr(0, nl);
        text = nl == std::string_view::npos ? std::string_view() : text.substr(nl + 1);
        if (!line.empty() && line.back() == '\r') line.remove_suffix(1);
        if (line == "--------") {
            sections.emplace_back();
        } else if (!line.empty()) {
            sections.back().emplace_back(line);
        }
    }

    ParsedItem item;
    std::vector<std::string> names;
    for (const std::string& line : sections[0]) {
        if (take_prefix(line, "Item Class: ", &item.item_class)) continue;
        if (take_prefix(line, "Rarity: ", &item.rarity)) continue;
        names.push_back(line);
    }
    // Some items have no rarity (Viscera limbs), but every item has a class.
    if ((item.rarity.empty() && item.item_class.empty()) || names.empty() || names.size() > 2) return std::nullopt;

    if (names.size() == 2) {
        item.name = names[0];
        item.base = names[1];
    } else {
        item.base = names[0];
    }
    sections.erase(sections.begin());
    for (const auto& section : sections)
        for (const std::string& line : section)
            if (item.item_level.empty()) take_prefix(line, "Item Level: ", &item.item_level);
    item.sections = std::move(sections);
    return item;
}

bool is_map_like(const ParsedItem& item) {
    static const char* const classes[] = {"Maps",       "Misc Map Items",   "Contracts",           "Heist Contracts",
                                          "Blueprints", "Heist Blueprints", "Expedition Logbooks", "Waystones"};
    if (item.item_class == "Maps" && item.rarity == "Unique") return false;
    for (const char* c : classes)
        if (item.item_class == c) return true;
    return false;
}
