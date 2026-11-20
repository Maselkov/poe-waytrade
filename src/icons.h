// Currency icons (PNG bytes) by trade currency id ("alch"), and listed items'
// images by item_icon_id(), fetched from the daemon. UI documents show them
// with <img src="icon:alch"/>, which the renderer resolves here.
#pragma once

#include <string>

void icons_put(const std::string& id, std::string png);
// nullptr until the icon has arrived.
const std::string* icons_get(const std::string& id);

// A short id for a listed item's image URL ("item-<hash>"), fit for a src.
std::string item_icon_id(const std::string& url);

// The daemon's id for the trade site's socket sprite sheet.
inline constexpr const char* kSocketSprite = "sockets";

// For the daemon's "png <base64>" replies.
std::string base64_decode(const std::string& in);
// For pixels sent to the daemon (MATCHART).
std::string base64_encode(const unsigned char* data, size_t size);
