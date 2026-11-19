#include "icons.h"

#include <map>

namespace {

// Only touched from the present thread (PriceCheck and the renderer).
std::map<std::string, std::string>& store() {
    static std::map<std::string, std::string> icons;
    return icons;
}

}  // namespace

void icons_put(const std::string& id, std::string png) { store()[id] = std::move(png); }

const std::string* icons_get(const std::string& id) {
    auto it = store().find(id);
    return it == store().end() ? nullptr : &it->second;
}

std::string base64_decode(const std::string& in) {
    auto value = [](char c) -> int {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '+') return 62;
        if (c == '/') return 63;
        return -1;
    };
    std::string out;
    out.reserve(in.size() / 4 * 3);
    unsigned buffer = 0;
    int bits = 0;
    for (char c : in) {
        int v = value(c);
        if (v < 0) continue;  // padding, line breaks
        buffer = (buffer << 6) | v;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out.push_back(char((buffer >> bits) & 0xFF));
        }
    }
    return out;
}
