// Currency icons (PNG bytes) by trade currency id ("alch"), fetched from the
// daemon. UI documents show them with <img src="icon:alch"/>, which the
// renderer resolves here.
#pragma once

#include <string>

void icons_put(const std::string& id, std::string png);
// nullptr until the icon has arrived.
const std::string* icons_get(const std::string& id);

// For the daemon's "png <base64>" replies.
std::string base64_decode(const std::string& in);
