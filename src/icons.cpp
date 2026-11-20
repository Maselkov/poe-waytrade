#include "icons.h"

#include <cstdint>
#include <cstdio>
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

std::string item_icon_id(const std::string& url) {
    uint64_t h = 0xcbf29ce484222325ull;  // FNV-1a
    for (unsigned char c : url) h = (h ^ c) * 0x100000001b3ull;
    char id[24];
    snprintf(id, sizeof id, "item-%016llx", (unsigned long long)h);
    return id;
}

std::string base64_encode(const unsigned char* data, size_t size) {
    static const char table[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string out;
    out.reserve((size + 2) / 3 * 4);
    for (size_t i = 0; i < size; i += 3) {
        unsigned n = unsigned(data[i]) << 16;
        if (i + 1 < size) n |= unsigned(data[i + 1]) << 8;
        if (i + 2 < size) n |= data[i + 2];
        out.push_back(table[(n >> 18) & 63]);
        out.push_back(table[(n >> 12) & 63]);
        out.push_back(i + 1 < size ? table[(n >> 6) & 63] : '=');
        out.push_back(i + 2 < size ? table[n & 63] : '=');
    }
    return out;
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
