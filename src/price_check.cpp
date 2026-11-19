#include "price_check.h"
#include "icons.h"
#include "layer.h"
#include "wl_input.h"

#include <linux/input-event-codes.h>

#include <algorithm>
#include <utility>
#include <cstdio>
#include <cstdlib>
#include <fstream>

namespace {

// PoE copies the item within a frame or two; this is generous.
constexpr auto kCopyTimeout = std::chrono::milliseconds(1000);
// How long Ctrl+D waits for the clipboard to be saved before copying anyway
// (the save can still finish, and the restore waits for it). Text arrives in
// a few ms; big images take longer.
constexpr auto kClipboardSaveWait = std::chrono::milliseconds(150);
// A quick panel closes once the mouse is this far (logical px) from where
// the hotkey was pressed, unless Ctrl is held.
constexpr float kQuickCloseDistance = 40.0f;
// A pinned panel closes once the pointer has been off it this long.
constexpr auto kPinnedLeaveGrace = std::chrono::milliseconds(250);

// Ctrl+D on a map opens the map check (is it safe to run?); logbooks are
// mostly about what they're worth.
PriceCheck::Purpose default_purpose(const ParsedItem& item) {
    if (!is_map_like(item) || item.item_class == "Expedition Logbooks") return PriceCheck::Purpose::Price;
    return PriceCheck::Purpose::Map;
}

}  // namespace

PriceCheck::PriceCheck() {
    wake_.ping();
    test_copy_ = getenv("POE_WAYTRADE_TEST_COPY") != nullptr;
    // For working on the UI without the game: open the panel on start with
    // an item as if it had just been copied.
    if (const char* path = getenv("POE_WAYTRADE_DEMO_ITEM")) {
        std::ifstream f(path);
        item_text_.assign(std::istreambuf_iterator<char>(f), {});
        item_ = parse_item_text(item_text_);
        status_ = item_ ? "" : std::string("POE_WAYTRADE_DEMO_ITEM isn't PoE item text: ") + path;
        pending_purpose_ = item_ ? default_purpose(*item_) : Purpose::Price;
        // POE_WAYTRADE_DEMO_VIEW=price: a map opens on its price instead.
        const char* view = getenv("POE_WAYTRADE_DEMO_VIEW");
        if (view && std::string(view) == "price") pending_purpose_ = Purpose::Price;
        // POE_WAYTRADE_DEMO_PRESET=base: open on the "Base item" preset.
        const char* preset = getenv("POE_WAYTRADE_DEMO_PRESET");
        if (item_ && preset) daemon_.request_prepare(preset, item_text_);
        else if (item_) pending_purpose_ == Purpose::Map ? daemon_.request_map(item_text_) : daemon_.request(item_text_);
        pending_mode_ = Mode::Locked;
        demo_ = true;
        open();
    }
}

bool PriceCheck::anchor(float* x, float* y) const {
    if (!has_anchor_) return false;
    *x = anchor_x_;
    *y = anchor_y_;
    return true;
}

void PriceCheck::update(WaylandInput& input, Clock::time_point now) {
    if (release_pending_) {
        keyboard_.key(KEY_C, false);
        if (we_pressed_alt_) keyboard_.key(KEY_LEFTALT, false);
        if (we_pressed_ctrl_) keyboard_.key(KEY_LEFTCTRL, false);
        keyboard_.sync();
        release_pending_ = false;
    }

    if (test_copy_ && !test_copy_at_) test_copy_at_ = now + std::chrono::seconds(2);
    if (test_copy_ && test_copy_at_ && now > *test_copy_at_ && input.focused()) {
        test_copy_at_ = Clock::time_point::max();
        poe_log("test copy: Ctrl+D");
        request_copy(input, Mode::Locked, Purpose::Price);
    }

    for (Hotkey key : input.take_hotkeys()) {
        switch (key) {
        case Hotkey::PriceCheck:
            request_copy(input, Mode::Quick, Purpose::Price);
            break;
        case Hotkey::PriceCheckLocked:
            request_copy(input, Mode::Locked, Purpose::Price);
            break;
        case Hotkey::MapCheck:
            // Alt+D: the old map check key, now the same as Ctrl+Alt+D.
            request_copy(input, Mode::Locked, Purpose::Price);
            break;
        case Hotkey::CopySeen:
            // The user's own Ctrl+C on an item, or the echo of ours (then
            // we're already waiting and keep the hotkey's mode).
            if (!waiting_)
                begin_wait(input, Mode::Quick, Purpose::Price, now + kCopyTimeout);
            else
                deadline_ = now + kCopyTimeout;
            break;
        }
    }

    if (copy_queued_ && (input.selection_saved() || now > copy_queued_until_)) send_copy_keys(input);
    if (waiting_) wait_for_clipboard(input, now);
    apply_closing_rules(input, now);

    PriceReply reply;
    if (daemon_.poll(&reply)) {
        // A failed re-search (e.g. rate limited) keeps the filters on screen.
        if (reply.status == "error" && reply_ && reply_->status == "trade") {
            status_ = reply.message;
        } else {
            reply_ = std::move(reply);
            stale_ = false;
            request_icons();
            // POE_WAYTRADE_DEMO_SEARCH=1: a demo item searches by itself, so
            // the searched state can be checked without clicking.
            if (demo_ && reply_->status == "trade" && !reply_->searched && getenv("POE_WAYTRADE_DEMO_SEARCH")) search();
        }
        changed();
    }
    if (wake_.poll(&reply) && reply.status == "error") poe_log("daemon wake-up: %s", reply.message.c_str());
    if (opener_.poll(&reply) && reply.status != "ok") poe_log("open trade site: %s", reply.message.c_str());
    if (sender_.poll(&reply) && reply.status != "ok") poe_log("daemon: %s", reply.message.c_str());
    if (!sender_.busy() && !outbox_.empty()) {
        sender_.send_raw(outbox_.front());
        outbox_.pop_front();
    }
    if (icon_client_.poll(&reply)) {
        if (reply.status == "ok" && !reply.png.empty()) {
            icons_put(icon_pending_, base64_decode(reply.png));
            changed();  // the UI shows icons once they're here
        }
        icon_pending_.clear();
    }
    if (!icon_client_.busy() && icon_pending_.empty() && !icon_queue_.empty()) {
        icon_pending_ = icon_queue_.front();
        icon_queue_.pop_front();
        icon_client_.send_raw("ICON\n" + icon_pending_);
    }
}

void PriceCheck::apply_closing_rules(WaylandInput& input, Clock::time_point now) {
    int game_clicks = input.take_game_clicks();
    if (!visible_ || demo_) return;
    switch (mode_) {
    case Mode::Quick: {
        if (input.pointer_inside()) {
            mode_ = Mode::Pinned;
            left_panel_at_.reset();
            changed();
            break;
        }
        float x, y;
        if (!input.ctrl_held() && has_anchor_ && input.game_pointer(&x, &y)) {
            float limit = kQuickCloseDistance * (float)input.scale();
            float dx = x - anchor_x_, dy = y - anchor_y_;
            if (dx * dx + dy * dy > limit * limit) set_visible(false);
        }
        break;
    }
    case Mode::Pinned:
        // Holding Ctrl keeps it open wherever the pointer goes.
        if (input.pointer_inside() || input.ctrl_held()) {
            left_panel_at_.reset();
        } else if (!left_panel_at_) {
            left_panel_at_ = now;
        } else if (now - *left_panel_at_ > kPinnedLeaveGrace) {
            set_visible(false);
        }
        break;
    case Mode::Locked:
        if (game_clicks > 0) set_visible(false);
        break;
    }
}

void PriceCheck::request_copy(WaylandInput& input, Mode mode, Purpose purpose) {
    if (!keyboard_.ok()) {
        status_ = keyboard_.error() + ". Press Ctrl+C on the item instead.";
        item_.reset();
        pending_mode_ = Mode::Locked;  // there's nothing to move toward
        pending_purpose_ = purpose;
        open();
        return;
    }
    // The copy replaces the user's clipboard: save it first (or give up
    // waiting after a moment), and put it back afterwards.
    input.save_selection();
    copy_queued_ = true;
    copy_queued_until_ = Clock::now() + kClipboardSaveWait;
    pending_mode_ = mode;
    pending_purpose_ = purpose;
    if (input.selection_saved()) send_copy_keys(input);
}

void PriceCheck::send_copy_keys(WaylandInput& input) {
    copy_queued_ = false;
    restore_clipboard_ = true;
    Mode mode = pending_mode_;
    Purpose purpose = pending_purpose_;
    // Ctrl+Alt+C: PoE's advanced copy, which adds each mod's roll range (for
    // the trade filters' sliders) and marks which lines are mods. The user is
    // still holding some modifiers from the hotkey; only add the others.
    if (test_copy_) {
        poe_log("test copy: would press Ctrl+Alt+C now");
        begin_wait(input, mode, purpose, Clock::now() + std::chrono::seconds(5));
        return;
    }
    we_pressed_ctrl_ = !input.ctrl_held();
    we_pressed_alt_ = !input.alt_held();
    if (we_pressed_ctrl_) keyboard_.key(KEY_LEFTCTRL, true);
    if (we_pressed_alt_) keyboard_.key(KEY_LEFTALT, true);
    keyboard_.key(KEY_C, true);
    keyboard_.sync();
    release_pending_ = true;
    begin_wait(input, mode, purpose, Clock::now() + kCopyTimeout);
}

void PriceCheck::begin_wait(WaylandInput& input, Mode mode, Purpose purpose, Clock::time_point deadline) {
    waiting_ = true;
    seq_at_request_ = input.clipboard_seq();
    deadline_ = deadline;
    pending_mode_ = mode;
    pending_purpose_ = purpose;
    pending_anchor_ = input.game_pointer(&pending_x_, &pending_y_);
}

void PriceCheck::wait_for_clipboard(WaylandInput& input, Clock::time_point now) {
    if (input.clipboard_seq() != seq_at_request_) {
        waiting_ = false;
        if (std::exchange(restore_clipboard_, false)) input.restore_selection();
        item_ = parse_item_text(input.clipboard_text());
        status_ = item_ ? "" : "Copied text isn't a PoE item";
        reply_.reset();
        other_reply_.reset();
        item_text_ = input.clipboard_text();
        if (item_) pending_purpose_ = default_purpose(*item_);
        if (item_) pending_purpose_ == Purpose::Map ? daemon_.request_map(item_text_) : daemon_.request(item_text_);
        open();
    } else if (now > deadline_) {
        waiting_ = false;
        if (std::exchange(restore_clipboard_, false)) input.forget_saved_selection();
        item_.reset();
        reply_.reset();
        status_ = "Nothing was copied. Is the cursor over an item?";
        open();
    }
}

// Shows the panel for a new check, in the mode and at the place the hotkey
// asked for.
void PriceCheck::open() {
    demo_ = demo_ && check_id_ == 0;  // a real check ends the demo
    mode_ = pending_mode_;
    purpose_ = pending_purpose_;
    has_anchor_ = pending_anchor_;
    anchor_x_ = pending_x_;
    anchor_y_ = pending_y_;
    check_id_++;
    visible_ = true;
    changed();
}

void PriceCheck::set_visible(bool v) {
    if (visible_ == v) return;
    visible_ = v;
    if (getenv("POE_WAYTRADE_DEBUG")) poe_log("price check: %s (mode %d)", v ? "shown" : "hidden", (int)mode_);
    changed();
}

// Nothing is searched until the user asks: every search counts against
// GGG's rate limit.
void PriceCheck::toggle_filter(int index) {
    if (!reply_ || reply_->status != "trade") return;
    for (TradeFilter& f : reply_->filters)
        if (f.index == index) f.enabled = !f.enabled;
    stale_ = reply_->searched;
    changed();
}

void PriceCheck::set_bounds(int index, double min, double max) {
    if (!reply_ || reply_->status != "trade") return;
    for (TradeFilter& f : reply_->filters) {
        if (f.index != index) continue;
        auto text = [&](double v) {
            if (v < 0) return std::string();
            char buf[32];
            snprintf(buf, sizeof(buf), f.step < 1 ? "%.2f" : "%.0f", v);
            return std::string(buf);
        };
        std::string min_text = text(min), max_text = text(max);
        if (f.min == min_text && f.max == max_text) return;
        f.min = min_text;
        f.max = max_text;
    }
    stale_ = reply_->searched;
    changed();
}

bool PriceCheck::can_switch_view() const {
    return item_ && is_map_like(*item_);
}

void PriceCheck::switch_view() {
    // Not while a search is out: its answer would land in the wrong view.
    if (!can_switch_view() || daemon_.busy()) return;
    purpose_ = purpose_ == Purpose::Map ? Purpose::Price : Purpose::Map;
    std::swap(reply_, other_reply_);
    std::swap(stale_, other_stale_);
    status_.clear();
    if (!reply_) purpose_ == Purpose::Map ? daemon_.request_map(item_text_) : daemon_.request(item_text_);
    // The panel changes size; a quick look could slip out from under the
    // pointer and close.
    mode_ = Mode::Locked;
    changed();
}

void PriceCheck::request_icons() {
    if (!reply_) return;
    auto want = [&](const std::string& id) {
        if (id.empty() || icons_get(id) || !icons_requested_.insert(id).second) return;
        icon_queue_.push_back(id);
    };
    if (reply_->summary) want(reply_->summary->currency_id);
    // The poe.ninja card on a unique's trade panel
    if (reply_->status == "trade" && !reply_->prices.empty())
        want(reply_->prices[0].divine >= 1.0 ? "divine" : "chaos");
    for (const TradeListing& l : reply_->listings) want(l.currency_id);
}

void PriceCheck::set_base_only(bool base_only) {
    if (!reply_ || reply_->status != "trade" || reply_->base_only == base_only) return;
    reply_->base_only = base_only;
    stale_ = reply_->searched;
    changed();
}

void PriceCheck::toggle_preset() {
    if (!reply_ || reply_->status != "trade" || !reply_->base_preset || daemon_.busy()) return;
    status_.clear();
    daemon_.request_prepare(reply_->preset == "base" ? "normal" : "base", item_text_);
    changed();
}

void PriceCheck::cycle_corrupted() {
    if (!reply_ || reply_->status != "trade") return;
    std::string& c = reply_->corrupted;
    c = c == "any" ? "no" : c == "no" ? "yes" : "any";
    stale_ = reply_->searched;
    changed();
}

void PriceCheck::search() {
    if (!reply_ || reply_->status != "trade" || daemon_.busy()) return;
    std::vector<TradeFilter> enabled;
    for (const TradeFilter& f : reply_->filters)
        if (f.enabled) enabled.push_back(f);
    status_.clear();
    daemon_.request_trade(enabled, reply_->base_only, reply_->corrupted, reply_->preset, item_text_);
    changed();
}

void PriceCheck::cycle_mod(int index, bool backward) {
    if (!reply_ || reply_->status != "map" || index < 0 || index >= (int)reply_->map_mods.size()) return;
    MapMod& mod = reply_->map_mods[index];
    size_t slot = std::clamp(reply_->profile, 1, 3) - 1;
    while (mod.marks.size() < 3) mod.marks += '-';
    static const std::string order = "-dwg";
    size_t at = order.find(mod.marks[slot]);
    if (at == std::string::npos) at = 0;
    char next = order[(at + (backward ? order.size() - 1 : 1)) % order.size()];
    mod.marks[slot] = next;
    outbox_.push_back("MAPMARK\n" + std::to_string(slot + 1) + "\n" + next + "\n" + mod.key);
    changed();
}

void PriceCheck::set_profile(int profile) {
    if (!reply_ || reply_->status != "map" || profile < 1 || profile > 3 || profile == reply_->profile) return;
    reply_->profile = profile;
    outbox_.push_back("MAPPROFILE\n" + std::to_string(profile));
    changed();
}

void PriceCheck::open_trade() {
    if (reply_ && !reply_->url.empty()) opener_.open_url(reply_->url);
}
