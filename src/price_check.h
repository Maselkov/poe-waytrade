// Price-check flow: hotkey -> Ctrl+C -> wait for clipboard -> parse the item
// -> ask the daemon for prices. The UI (ui.cpp) reads this state and calls
// the actions below.
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
    PriceCheck();

    // Once per frame.
    void update(WaylandInput& input, Clock::time_point now);

    // State for the UI. revision() changes whenever anything below does.
    uint64_t revision() const { return revision_; }
    bool visible() const { return visible_; }
    const std::optional<ParsedItem>& item() const { return item_; }
    const std::optional<PriceReply>& reply() const { return reply_; }
    const std::string& status() const { return status_; }  // error/progress text
    bool searching() const { return daemon_.busy(); }

    // Actions from the UI.
    void close() { set_visible(false); }
    void toggle_filter(int index);
    void open_trade();

private:
    void request_copy(WaylandInput& input);
    void wait_for_clipboard(WaylandInput& input, Clock::time_point now);
    void set_visible(bool v);
    void changed() { revision_++; }

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

    uint64_t revision_ = 0;
    bool visible_ = false;
    std::optional<ParsedItem> item_;
    std::string status_;
};
