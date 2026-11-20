#include "hotkeys.h"

#include <linux/input-event-codes.h>

#include <cstdlib>
#include <sstream>
#include <vector>

namespace {

struct Mod {
    uint32_t bit;
    const char* name;   // in the settings format
    const char* label;  // shown
};
constexpr Mod kMods[] = {
    {kModCtrl, "ctrl", "Ctrl"},
    {kModAlt, "alt", "Alt"},
    {kModShift, "shift", "Shift"},
    {kModSuper, "super", "Super"},
};

const char* key_name(uint32_t code) {
    switch (code) {
    case KEY_ESC: return "Esc";
    case KEY_1: return "1";
    case KEY_2: return "2";
    case KEY_3: return "3";
    case KEY_4: return "4";
    case KEY_5: return "5";
    case KEY_6: return "6";
    case KEY_7: return "7";
    case KEY_8: return "8";
    case KEY_9: return "9";
    case KEY_0: return "0";
    case KEY_MINUS: return "-";
    case KEY_EQUAL: return "=";
    case KEY_BACKSPACE: return "Backspace";
    case KEY_TAB: return "Tab";
    case KEY_Q: return "Q";
    case KEY_W: return "W";
    case KEY_E: return "E";
    case KEY_R: return "R";
    case KEY_T: return "T";
    case KEY_Y: return "Y";
    case KEY_U: return "U";
    case KEY_I: return "I";
    case KEY_O: return "O";
    case KEY_P: return "P";
    case KEY_LEFTBRACE: return "[";
    case KEY_RIGHTBRACE: return "]";
    case KEY_ENTER: return "Enter";
    case KEY_A: return "A";
    case KEY_S: return "S";
    case KEY_D: return "D";
    case KEY_F: return "F";
    case KEY_G: return "G";
    case KEY_H: return "H";
    case KEY_J: return "J";
    case KEY_K: return "K";
    case KEY_L: return "L";
    case KEY_SEMICOLON: return ";";
    case KEY_APOSTROPHE: return "'";
    case KEY_GRAVE: return "`";
    case KEY_BACKSLASH: return "\\";
    case KEY_Z: return "Z";
    case KEY_X: return "X";
    case KEY_C: return "C";
    case KEY_V: return "V";
    case KEY_B: return "B";
    case KEY_N: return "N";
    case KEY_M: return "M";
    case KEY_COMMA: return ",";
    case KEY_DOT: return ".";
    case KEY_SLASH: return "/";
    case KEY_KPASTERISK: return "Num *";
    case KEY_SPACE: return "Space";
    case KEY_CAPSLOCK: return "Caps Lock";
    case KEY_F1: return "F1";
    case KEY_F2: return "F2";
    case KEY_F3: return "F3";
    case KEY_F4: return "F4";
    case KEY_F5: return "F5";
    case KEY_F6: return "F6";
    case KEY_F7: return "F7";
    case KEY_F8: return "F8";
    case KEY_F9: return "F9";
    case KEY_F10: return "F10";
    case KEY_F11: return "F11";
    case KEY_F12: return "F12";
    case KEY_KP7: return "Num 7";
    case KEY_KP8: return "Num 8";
    case KEY_KP9: return "Num 9";
    case KEY_KPMINUS: return "Num -";
    case KEY_KP4: return "Num 4";
    case KEY_KP5: return "Num 5";
    case KEY_KP6: return "Num 6";
    case KEY_KPPLUS: return "Num +";
    case KEY_KP1: return "Num 1";
    case KEY_KP2: return "Num 2";
    case KEY_KP3: return "Num 3";
    case KEY_KP0: return "Num 0";
    case KEY_KPDOT: return "Num .";
    case KEY_KPENTER: return "Num Enter";
    case KEY_KPSLASH: return "Num /";
    case KEY_102ND: return "<";
    case KEY_HOME: return "Home";
    case KEY_UP: return "Up";
    case KEY_PAGEUP: return "Page Up";
    case KEY_LEFT: return "Left";
    case KEY_RIGHT: return "Right";
    case KEY_END: return "End";
    case KEY_DOWN: return "Down";
    case KEY_PAGEDOWN: return "Page Down";
    case KEY_INSERT: return "Insert";
    case KEY_DELETE: return "Delete";
    case KEY_PAUSE: return "Pause";
    case KEY_SCROLLLOCK: return "Scroll Lock";
    case KEY_SYSRQ: return "Print";
    case KEY_COMPOSE: return "Menu";
    default: return nullptr;
    }
}

}  // namespace

uint32_t key_mod(uint32_t code) {
    switch (code) {
    case KEY_LEFTCTRL:
    case KEY_RIGHTCTRL: return kModCtrl;
    case KEY_LEFTALT:
    case KEY_RIGHTALT: return kModAlt;
    case KEY_LEFTSHIFT:
    case KEY_RIGHTSHIFT: return kModShift;
    case KEY_LEFTMETA:
    case KEY_RIGHTMETA: return kModSuper;
    default: return 0;
    }
}

KeyCombo parse_combo(const std::string& s) {
    KeyCombo c;
    std::vector<std::string> parts;
    std::stringstream ss(s);
    for (std::string part; std::getline(ss, part, '+');) parts.push_back(part);
    if (parts.empty()) return c;
    for (size_t i = 0; i + 1 < parts.size(); i++)
        for (const Mod& m : kMods)
            if (parts[i] == m.name) c.mods |= m.bit;
    char* end = nullptr;
    unsigned long code = strtoul(parts.back().c_str(), &end, 10);
    if (end && *end == '\0' && code > 0 && !key_mod((uint32_t)code)) c.code = (uint32_t)code;
    return c;
}

std::string format_combo(const KeyCombo& c) {
    if (c.empty()) return "";
    std::string out;
    for (const Mod& m : kMods)
        if (c.mods & m.bit) out += std::string(m.name) + "+";
    return out + std::to_string(c.code);
}

std::string mods_label(uint32_t mods) {
    std::string out;
    for (const Mod& m : kMods)
        if (mods & m.bit) out += (out.empty() ? "" : " + ") + std::string(m.label);
    return out;
}

std::string combo_label(const KeyCombo& c) {
    if (c.empty()) return "";
    const char* name = key_name(c.code);
    std::string key = name ? name : "Key " + std::to_string(c.code);
    std::string mods = mods_label(c.mods);
    return mods.empty() ? key : mods + " + " + key;
}
