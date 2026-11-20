#include "stash_scroll.h"
#include "uinput_kbd.h"

#include <linux/input-event-codes.h>

#include <algorithm>

namespace {

// A fast flick shouldn't run on for seconds: at most this many taps wait.
constexpr int kMaxPending = 8;

}  // namespace

// APT's isStashArea: the left side panel (370/600 of the height wide, as
// in PoE's 800x600 layout; the same 986/1600 the panels are placed by),
// from the tab bar down to the bottom of the grid.
bool StashScroll::over_stash(float x, float y, int height) {
    if (height <= 0) return false;
    float h = (float)height;
    return x <= h * 370.0f / 600.0f && y > h * 154.0f / 1600.0f && y < h * 1192.0f / 1600.0f;
}

void StashScroll::update(WaylandInput& input, UinputKeyboard& keyboard, int height, bool enabled) {
    // One tap per two frames: pressed in one, released in the next, so the
    // game sees each press as its own key event.
    if (held_) {
        keyboard.key(held_, false);
        keyboard.sync();
        held_ = 0;
    }

    for (const WaylandInput::GameWheel& w : input.take_game_wheel()) {
        // Only with Ctrl alone (Shift and Super don't matter): Alt+arrows
        // could be something else, and Ctrl+Alt+wheel stays the game's.
        if (!enabled || !keyboard.ok() || (w.mods & (kModCtrl | kModAlt)) != kModCtrl) continue;
        // The arrows go to the focused window, which may not be the game
        // when the cursor is only passing over it.
        if (!input.focused() || over_stash(w.x, w.y, height)) continue;
        pending_ = std::clamp(pending_ + w.steps, -kMaxPending, kMaxPending);
    }
    if (!enabled || !input.focused()) pending_ = 0;

    if (pending_ != 0) {
        held_ = pending_ > 0 ? KEY_RIGHT : KEY_LEFT;
        pending_ += pending_ > 0 ? -1 : 1;
        keyboard.key(held_, true);
        keyboard.sync();
    }
}
