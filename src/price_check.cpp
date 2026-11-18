#include "price_check.h"
#include "layer.h"
#include "wl_input.h"

#include <linux/input-event-codes.h>

#include <cstdlib>
#include <fstream>

namespace {

// PoE copies the item within a frame or two; this is generous.
constexpr auto kCopyTimeout = std::chrono::milliseconds(1000);

}  // namespace

PriceCheck::PriceCheck() {
    wake_.ping();
    // For working on the UI without the game: open the panel on start with
    // an item as if it had just been copied.
    if (const char* path = getenv("POE_OVERLAY_DEMO_ITEM")) {
        std::ifstream f(path);
        item_text_.assign(std::istreambuf_iterator<char>(f), {});
        item_ = parse_item_text(item_text_);
        status_ = item_ ? "" : std::string("POE_OVERLAY_DEMO_ITEM isn't PoE item text: ") + path;
        if (item_) daemon_.request(item_text_);
        visible_ = true;
        changed();
    }
}

void PriceCheck::update(WaylandInput& input, Clock::time_point now) {
    if (release_pending_) {
        keyboard_.key(KEY_C, false);
        if (we_pressed_ctrl_) keyboard_.key(KEY_LEFTCTRL, false);
        keyboard_.sync();
        release_pending_ = false;
    }

    for (Hotkey key : input.take_hotkeys()) {
        switch (key) {
        case Hotkey::PriceCheck:
            request_copy(input);
            break;
        case Hotkey::CopySeen:
            // The user's own Ctrl+C (or the echo of ours) on an item.
            if (!waiting_) {
                waiting_ = true;
                seq_at_request_ = input.clipboard_seq();
            }
            deadline_ = now + kCopyTimeout;
            break;
        case Hotkey::Close:
            set_visible(false);
            break;
        }
    }

    if (waiting_) wait_for_clipboard(input, now);

    PriceReply reply;
    if (daemon_.poll(&reply)) {
        // A failed re-search (e.g. rate limited) keeps the filters on screen.
        if (reply.status == "error" && reply_ && reply_->status == "trade")
            status_ = reply.message;
        else
            reply_ = std::move(reply);
        changed();
    }
    if (wake_.poll(&reply) && reply.status == "error") poe_log("daemon wake-up: %s", reply.message.c_str());
    if (opener_.poll(&reply) && reply.status != "ok") poe_log("open trade site: %s", reply.message.c_str());
}

void PriceCheck::request_copy(WaylandInput& input) {
    if (!keyboard_.ok()) {
        status_ = keyboard_.error() + ". Press Ctrl+C on the item instead.";
        item_.reset();
        set_visible(true);
        changed();
        return;
    }
    // The user is still holding Ctrl from Ctrl+D; only add our own if not.
    we_pressed_ctrl_ = !input.ctrl_held();
    if (we_pressed_ctrl_) keyboard_.key(KEY_LEFTCTRL, true);
    keyboard_.key(KEY_C, true);
    keyboard_.sync();
    release_pending_ = true;

    waiting_ = true;
    seq_at_request_ = input.clipboard_seq();
    deadline_ = Clock::now() + kCopyTimeout;
}

void PriceCheck::wait_for_clipboard(WaylandInput& input, Clock::time_point now) {
    if (input.clipboard_seq() != seq_at_request_) {
        waiting_ = false;
        item_ = parse_item_text(input.clipboard_text());
        status_ = item_ ? "" : "Copied text isn't a PoE item";
        reply_.reset();
        item_text_ = input.clipboard_text();
        if (item_) daemon_.request(item_text_);
        set_visible(true);
        changed();
    } else if (now > deadline_) {
        waiting_ = false;
        item_.reset();
        reply_.reset();
        status_ = "Nothing was copied. Is the cursor over an item?";
        set_visible(true);
        changed();
    }
}

void PriceCheck::set_visible(bool v) {
    if (visible_ == v) return;
    visible_ = v;
    changed();
}

void PriceCheck::toggle_filter(int index) {
    if (!reply_ || reply_->status != "trade") return;
    std::vector<int> enabled;
    for (TradeFilter& f : reply_->filters) {
        if (f.index == index) f.enabled = !f.enabled;
        if (f.enabled) enabled.push_back(f.index);
    }
    status_.clear();
    daemon_.request_trade(enabled, item_text_);
    changed();
}

void PriceCheck::open_trade() {
    if (reply_ && !reply_->url.empty()) opener_.open_url(reply_->url);
}
