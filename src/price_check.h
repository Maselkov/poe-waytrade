// Item check flow: hotkey -> Ctrl+C -> wait for clipboard -> parse the item
// -> ask the daemon for prices (Ctrl+D) or a map mod check (Alt+D). The UI
// (ui.cpp) reads this state and calls the actions below.
#pragma once

#include "daemon_client.h"
#include "item_text.h"
#include "uinput_kbd.h"

#include <chrono>
#include <cstdint>
#include <deque>
#include <set>
#include <optional>
#include <string>

class WaylandInput;

class PriceCheck {
public:
    using Clock = std::chrono::steady_clock;

    // How the panel behaves once open (modelled on Awakened PoE Trade):
    //   Quick:  Ctrl+D. Closes when the mouse moves away, unless Ctrl is
    //           held; moving onto the panel while holding Ctrl pins it.
    //   Pinned: a quick panel the mouse moved onto. Closes when it leaves,
    //           unless Ctrl is held.
    //   Locked: Ctrl+Alt+D. Stays until its close button or a click on the
    //           game.
    enum class Mode { Quick, Pinned, Locked };
    // What the check is for: a price, or the map mod danger check.
    enum class Purpose { Price, Map };

    // Created with the game's device: wakes the daemon right away.
    PriceCheck();

    // Once per frame.
    void update(WaylandInput& input, Clock::time_point now);

    // State for the UI. revision() changes whenever anything below does.
    uint64_t revision() const { return revision_; }
    bool visible() const { return visible_; }
    Mode mode() const { return mode_; }
    Purpose purpose() const { return purpose_; }
    // Bumped each time the panel opens for a new check; with anchor(), where
    // the cursor was (swapchain pixels), for placing the panel beside it.
    uint64_t check_id() const { return check_id_; }
    bool anchor(float* x, float* y) const;
    const std::optional<ParsedItem>& item() const { return item_; }
    const std::optional<PriceReply>& reply() const { return reply_; }
    const std::string& status() const { return status_; }  // error/progress text
    bool searching() const { return daemon_.busy(); }

    // Actions from the UI.
    void close() { set_visible(false); }
    // Trade filters and options only change locally; search() sends them.
    // stale(): they changed since the listings on screen were searched.
    void toggle_filter(int index);
    void set_min(int index, double min);
    void set_base_only(bool base_only);
    void cycle_corrupted();  // any -> no -> yes
    void search();
    bool stale() const { return stale_; }
    void open_trade();
    // Map check: cycle the mod at this index in reply().map_mods through
    // unmarked -> deadly -> warning -> good (or backward) in the active
    // profile, and
    // switch the active profile (1-3). Both are saved by the daemon.
    void cycle_mod(int index, bool backward = false);
    void set_profile(int profile);

private:
    void request_copy(WaylandInput& input, Mode mode, Purpose purpose);
    void begin_wait(WaylandInput& input, Mode mode, Purpose purpose, Clock::time_point deadline);
    void wait_for_clipboard(WaylandInput& input, Clock::time_point now);
    void open();
    void apply_closing_rules(WaylandInput& input, Clock::time_point now);
    void set_visible(bool v);
    void changed() { revision_++; }

    UinputKeyboard keyboard_;
    bool release_pending_ = false;  // keys go down one frame, up the next
    bool we_pressed_ctrl_ = false;
    bool we_pressed_alt_ = false;

    bool waiting_ = false;
    uint64_t seq_at_request_ = 0;
    Clock::time_point deadline_;

    DaemonClient daemon_;
    DaemonClient wake_;    // reply ignored
    DaemonClient opener_;  // "open on trade site"; reply ignored
    // One-off saves (map marks, profile) go out one at a time, in order.
    DaemonClient sender_;
    std::deque<std::string> outbox_;
    // Currency icons for the listings, fetched one at a time.
    void request_icons();
    DaemonClient icon_client_;
    std::deque<std::string> icon_queue_;
    std::string icon_pending_;
    std::set<std::string> icons_requested_;
    std::optional<PriceReply> reply_;
    std::string item_text_;  // as copied, for re-running trade searches

    // The check being waited for: how to open, and where the cursor was.
    Mode pending_mode_ = Mode::Quick;
    Purpose pending_purpose_ = Purpose::Price;
    bool pending_anchor_ = false;
    float pending_x_ = 0, pending_y_ = 0;

    uint64_t revision_ = 0;
    uint64_t check_id_ = 0;
    Mode mode_ = Mode::Quick;
    Purpose purpose_ = Purpose::Price;
    bool has_anchor_ = false;
    float anchor_x_ = 0, anchor_y_ = 0;
    bool visible_ = false;
    bool stale_ = false;
    bool demo_ = false;  // POE_OVERLAY_DEMO_ITEM: stays open for design work
    // When the pointer left a pinned panel; it closes once that's long enough
    // ago, so slipping over the edge or the panel resizing doesn't close it.
    std::optional<Clock::time_point> left_panel_at_;
    std::optional<ParsedItem> item_;
    std::string status_;
};
