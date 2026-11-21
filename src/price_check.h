// Item check flow: hotkey -> Ctrl+C -> wait for clipboard -> parse the item
// -> ask the daemon for prices, or a map mod check for maps. The UI
// (ui.cpp) reads this state and calls the actions below.
#pragma once

#include "daemon_client.h"
#include "settings_page.h"
#include "item_text.h"
#include "stash_scroll.h"
#include "uinput_kbd.h"
#include "wl_grab.h"

#include <chrono>
#include <cstdint>
#include <deque>
#include <set>
#include <optional>
#include <string>
#include <utility>
#include <vector>

class WaylandInput;

// An inventory cell's pitch as a fraction of the frame's height: PoE1's UI
// scales with the height, and 1080p cells are 52.67 px apart. (Quad stash
// tabs' cells are half that.)
inline constexpr float kCellPerHeight = 52.67f / 1080.0f;

// The game's frame around the cursor when an item was checked, copied by
// overlay.cpp before the overlay draws: unidentified uniques show their own
// art, so the daemon can tell which one an item is (daemon/src/art.rs).
struct FrameCapture {
    int width = 0, height = 0;
    std::vector<uint8_t> rgb;  // rows top to bottom
    float cursor_x = 0, cursor_y = 0;
    float cell = 0;  // an inventory cell's pitch, in these pixels
};

class PriceCheck {
public:
    using Clock = std::chrono::steady_clock;

    // How the panel behaves once open (modelled on Awakened PoE Trade):
    //   Quick:  Ctrl+D. Closes when the mouse moves away, unless Ctrl is
    //           held; moving onto the panel while holding Ctrl pins it.
    //   Pinned: a quick panel the mouse moved onto. Closes when it leaves,
    //           unless Ctrl is held.
    //   Locked: Ctrl+Alt+D, or a quick/pinned panel that was clicked. Stays
    //           until its close button or a click on the game.
    enum class Mode { Quick, Pinned, Locked };
    // What the panel shows: a price, or the map mod danger check. Ctrl+D
    // picks from the item (maps, contracts and invitations open the map
    // check; everything else, logbooks included, the price) and
    // switch_view() flips between them on map-like items.
    enum class Purpose { Price, Map };

    // Created with the game's device: wakes the daemon right away.
    PriceCheck();

    // Once per frame. `frame_height`: the game's swapchain height.
    void update(WaylandInput& input, Clock::time_point now, int frame_height);

    // State for the UI. revision() changes whenever anything below does.
    uint64_t revision() const { return revision_ + settings_.revision(); }
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
    // The daemon's answer to the start-up ping: "" (none yet), ready,
    // loading (prices still coming), or error with daemon_error().
    const std::string& daemon_state() const { return daemon_state_; }
    const std::string& daemon_error() const { return daemon_error_; }

    // overlay.cpp: whether a check wants the frame around the cursor, and
    // where (swapchain pixels). `id` goes back with capture_taken().
    bool capture_wanted(uint64_t* id, float* x, float* y) const;
    // The copy is on its way (it takes a frame or two), or can't be made.
    void capture_started() { capture_wanted_ = false; }
    void capture_taken(uint64_t id, FrameCapture capture);

    // Fetches a currency icon (icons.h) if it isn't here yet: the display
    // scan's labels show chaos and divine ones.
    void want_icon(const std::string& id);

    // Presses of the display scan's hotkey since last asked (screen_scan.h).
    int take_displays_toggles() { return std::exchange(displays_toggles_, 0); }

    // The settings page (its own hotkey, or the gear on the panel).
    SettingsPage& settings() { return settings_; }
    const SettingsPage& settings() const { return settings_; }
    void open_settings();
    // The quick look hotkey's modifiers: holding them keeps a quick panel
    // open (Ctrl, by default).
    uint32_t hold_mods() const { return settings_.values().hotkeys[0].mods & ~(kModShift | kModSuper); }
    // Whether typing into panels works here (layer-shell; connects once).
    bool typing_supported() { return grab_.supported(); }

    // Actions from the UI.
    void close() { set_visible(false); }
    // A click on the panel: the user is working in it, so it stays until
    // dismissed instead of closing when the pointer slips off.
    void lock();
    // Trade filters and options only change locally; search() sends them.
    // stale(): they changed since the listings on screen were searched.
    void toggle_filter(int index);
    // A filter's bounds from its slider; max < 0: no maximum.
    void set_bounds(int index, double min, double max);
    void set_base_only(bool base_only);
    void toggle_corrupted();  // the item's own state <-> any
    void toggle_six_link();   // 6-linked only <-> any links
    void toggle_unidentified();  // unidentified only <-> any (an unidentified item's)
    // An unidentified unique's list of candidates: check the item as the
    // unique of the price at this index (its trade search, like an
    // identified one's). Locks the panel, since you're using it.
    void pick_unique(int index);
    // The unique(s) an unidentified item's art matched ("A / B" when they
    // share it); empty if none.
    const std::string& art_matched() const { return art_matched_; }
    // Switch between the normal filters and APT's "Base item" preset (the
    // item as a crafting base); fetches that preset's filters.
    void toggle_preset();
    void search();
    bool stale() const { return stale_; }
    // Typing a filter's minimum (field 0) or maximum (field 1). Clicking
    // either half of its value starts (or moves) the edit and takes the
    // keyboard from the game (wl_grab.h); clicking the other half or Tab
    // switches between minimum and maximum, Enter applies, Esc or a click in
    // the game cancels. Without layer-shell, status() says so.
    void edit_bounds(int index, int field = 0);
    struct BoundsEdit {
        int index = -1;
        int field = 0;         // 0: minimum, 1: maximum
        std::string min, max;  // as typed
        bool fresh = true;     // the next digit replaces the field's text
    };
    const std::optional<BoundsEdit>& bounds_edit() const { return edit_; }
    bool keyboard_taken() const { return grab_.focused(); }
    void open_trade();
    // Map check: cycle the mod at this index in reply().map_mods through
    // unmarked -> deadly -> warning -> good (or backward) in the active
    // profile, and
    // switch the active profile (1-3). Both are saved by the daemon.
    void cycle_mod(int index, bool backward = false);
    void set_profile(int profile);
    // Map-like items: the other view (map check <-> price), kept once
    // fetched. Locks the panel, since you're using it.
    bool can_switch_view() const;
    void switch_view();

private:
    void request_copy(WaylandInput& input, Mode mode, Purpose purpose);
    void begin_wait(WaylandInput& input, Mode mode, Purpose purpose, Clock::time_point deadline);
    void wait_for_clipboard(WaylandInput& input, Clock::time_point now);
    void open();
    void apply_closing_rules(WaylandInput& input, int game_clicks, Clock::time_point now);
    void set_visible(bool v);
    void changed() { revision_++; }

    UinputKeyboard keyboard_;
    StashScroll stash_scroll_;
    bool release_pending_ = false;  // keys go down one frame, up the next
    bool we_pressed_ctrl_ = false;
    bool we_pressed_alt_ = false;  // PoE2's advanced copy is Ctrl+Alt+C
    // Ctrl+D: the copy keystroke waits (briefly) until the user's clipboard
    // is saved, and the clipboard is restored once the item text is in.
    void send_copy_keys(WaylandInput& input);
    bool copy_queued_ = false;
    Clock::time_point copy_queued_until_;
    bool restore_clipboard_ = false;
    // POE_WAYTRADE_TEST_COPY=1: one Ctrl+D two seconds in, without sending
    // keys (something else plays the game's copy), to test clipboard keeping.
    bool test_copy_ = false;
    std::optional<Clock::time_point> test_copy_at_;

    bool waiting_ = false;
    uint64_t seq_at_request_ = 0;
    Clock::time_point deadline_;

    DaemonClient daemon_;
    DaemonClient wake_;    // the start-up ping; see daemon_state()
    std::string daemon_state_, daemon_error_;
    Clock::time_point wake_at_;
    DaemonClient opener_;  // "open on trade site"; reply ignored
    // One-off saves (map marks, profile) go out one at a time, in order.
    DaemonClient sender_;
    std::deque<std::string> outbox_;
    // Currency icons and listed items' images, fetched one at a time, as
    // (icons.h id, ICON request).
    void request_icons();
    DaemonClient icon_client_;
    std::deque<std::pair<std::string, std::string>> icon_queue_;
    std::string icon_pending_;
    std::set<std::string> icons_requested_;
    std::optional<PriceReply> reply_;
    // The view not on screen, once fetched (see switch_view()).
    std::optional<PriceReply> other_reply_;
    bool other_stale_ = false;
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
    bool demo_ = false;  // POE_WAYTRADE_DEMO_ITEM: stays open for design work
    // When the pointer left a pinned panel; it closes once that's long enough
    // ago, so slipping over the edge or the panel resizing doesn't close it.
    std::optional<Clock::time_point> left_panel_at_;
    std::optional<ParsedItem> item_;
    std::string status_;

    // Telling an unidentified unique apart by its art: the frame around the
    // cursor at this check, sent with MATCHART once the price reply says
    // there's more than one art to tell apart.
    uint64_t capture_id_ = 0;
    bool capture_wanted_ = false;
    float capture_x_ = 0, capture_y_ = 0;
    std::optional<FrameCapture> capture_;  // capture_id_'s
    DaemonClient art_client_;
    bool art_sent_ = false;
    uint64_t art_check_ = 0;  // the check_id_ it was sent for
    std::string art_matched_;
    void match_art();
    // Checks the item again as the unique `name` (a name line added to its text).
    void check_as(const std::string& name);

    int displays_toggles_ = 0;
    SettingsPage settings_;
    KeyboardGrab grab_;
    bool grab_focused_ = false;  // as last shown
    std::optional<BoundsEdit> edit_;
    void handle_edit_keys();
    void end_edit(bool apply);
};
