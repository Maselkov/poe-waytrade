// Price-check flow: hotkey -> Ctrl+C -> wait for clipboard -> show the item
// -> ask the daemon for prices.
#pragma once

#include "daemon_client.h"
#include "item_text.h"
#include "uinput_kbd.h"

#include <chrono>
#include <cstdint>
#include <optional>
#include <string>

class WaylandInput;

class PriceCheck {
public:
    using Clock = std::chrono::steady_clock;

    // Created with the game's device: wakes the daemon right away.
    PriceCheck() { wake_.ping(); }

    // Once per frame: update() before ImGui::NewFrame(), draw() inside it.
    void update(WaylandInput& input, Clock::time_point now);
    void draw();

private:
    void request_copy(WaylandInput& input);
    void wait_for_clipboard(WaylandInput& input, Clock::time_point now);
    void draw_prices();
    void draw_trade();

    UinputKeyboard keyboard_;
    bool release_pending_ = false;  // keys go down one frame, up the next
    bool we_pressed_ctrl_ = false;

    bool waiting_ = false;
    uint64_t seq_at_request_ = 0;
    Clock::time_point deadline_;

    DaemonClient daemon_;
    DaemonClient wake_;    // reply ignored
    DaemonClient opener_;  // "open on trade site"; reply ignored
    std::optional<PriceReply> reply_;
    std::string item_text_;  // as copied, for re-running trade searches

    bool visible_ = false;
    std::optional<ParsedItem> item_;
    std::string status_;
};
