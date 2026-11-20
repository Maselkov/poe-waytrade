// Key combinations for the hotkeys: modifiers plus an evdev key code, the
// way wl_input.cpp sees keys. Codes are physical keys, so a hotkey stays on
// the same key whatever the keyboard layout; labels name the US key there.
#pragma once

#include <cstdint>
#include <string>

enum KeyMod : uint32_t {
    kModCtrl = 1,
    kModAlt = 2,
    kModShift = 4,
    kModSuper = 8,
};

struct KeyCombo {
    uint32_t mods = 0;  // KeyMod bits
    uint32_t code = 0;  // evdev; 0: none
    bool empty() const { return code == 0; }
    bool operator==(const KeyCombo&) const = default;
};

// The modifier bit for an evdev code, or 0 for other keys.
uint32_t key_mod(uint32_t code);

// "ctrl+alt+32" (the daemon's settings format) <-> KeyCombo; "" is none.
KeyCombo parse_combo(const std::string& s);
std::string format_combo(const KeyCombo& c);

// "Ctrl + Alt + D"; "" for none.
std::string combo_label(const KeyCombo& c);
// "Ctrl + Alt" for a modifier set; "" for none.
std::string mods_label(uint32_t mods);
