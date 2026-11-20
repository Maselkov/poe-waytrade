// Ctrl + mouse wheel switches stash tabs, like Awakened PoE Trade's
// "Stash scroll": each notch over the game taps an arrow key (Right for
// down, Left for up) on the uinput keyboard, which PoE takes as the
// next or previous tab. Over the stash itself the game already does this,
// so notches there are left to it, or every notch would skip two tabs.
//
// The wheel still reaches the game too; there's no taking it away from a
// client. Ctrl is the user's own, still held, so the game sees Ctrl+Arrow,
// as it does with APT.
#pragma once

#include "wl_input.h"

class UinputKeyboard;

class StashScroll {
public:
    // Once per frame. `height`: the game's swapchain height, which sets the
    // stash's place on screen. Off: notches are dropped (a tap already
    // pressed is still released).
    void update(WaylandInput& input, UinputKeyboard& keyboard, int height, bool enabled);

    // Whether a notch at (x, y) is over the stash's tabs and grid, for a
    // game `height` pixels high (swapchain pixels).
    static bool over_stash(float x, float y, int height);

private:
    int pending_ = 0;    // taps to send: positive Right, negative Left
    uint32_t held_ = 0;  // the arrow pressed last frame, released this one
};
