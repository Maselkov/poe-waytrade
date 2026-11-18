#include "price_check.h"
#include "layer.h"
#include "wl_input.h"

#include "imgui.h"

#include <linux/input-event-codes.h>

#include <cstdio>
#include <cstdlib>

namespace {

// PoE copies the item within a frame or two; this is generous.
constexpr auto kCopyTimeout = std::chrono::milliseconds(1000);

ImVec4 rarity_color(const std::string& rarity) {
    if (rarity == "Magic") return ImVec4(0.53f, 0.53f, 1.00f, 1);
    if (rarity == "Rare") return ImVec4(1.00f, 1.00f, 0.47f, 1);
    if (rarity == "Unique") return ImVec4(0.69f, 0.38f, 0.15f, 1);
    if (rarity == "Gem") return ImVec4(0.11f, 0.64f, 0.61f, 1);
    if (rarity == "Currency") return ImVec4(0.67f, 0.62f, 0.51f, 1);
    return ImVec4(0.78f, 0.78f, 0.78f, 1);
}

}  // namespace

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
            visible_ = false;
            break;
        }
    }

    if (waiting_) wait_for_clipboard(input, now);

    PriceReply reply;
    if (daemon_.poll(&reply)) {
        // A failed re-search (e.g. rate limited) keeps the filters on screen.
        if (reply.status == "error" && reply_ && reply_->status == "trade")
            reply_->message = reply.message;
        else
            reply_ = std::move(reply);
    }
    if (wake_.poll(&reply) && reply.status == "error") poe_log("daemon wake-up: %s", reply.message.c_str());
    if (opener_.poll(&reply) && reply.status != "ok") poe_log("open trade site: %s", reply.message.c_str());
}

void PriceCheck::request_copy(WaylandInput& input) {
    if (!keyboard_.ok()) {
        status_ = keyboard_.error() + " - press Ctrl+C on the item instead";
        item_.reset();
        visible_ = true;
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
        visible_ = true;
        reply_.reset();
        item_text_ = input.clipboard_text();
        if (item_) daemon_.request(item_text_);
    } else if (now > deadline_) {
        waiting_ = false;
        item_.reset();
        status_ = "Nothing was copied - is the cursor over an item?";
        visible_ = true;
    }
}

void PriceCheck::draw() {
    if (!visible_) return;

    ImGui::SetNextWindowPos(ImVec2(40, 40), ImGuiCond_FirstUseEver);
    ImGui::SetNextWindowBgAlpha(0.9f);
    ImGui::Begin("Price check", &visible_,
                 ImGuiWindowFlags_AlwaysAutoResize | ImGuiWindowFlags_NoCollapse |
                     ImGuiWindowFlags_NoSavedSettings | ImGuiWindowFlags_NoNav);
    ImGui::PushTextWrapPos(ImGui::GetFontSize() * 28);

    if (item_) {
        ImVec4 color = rarity_color(item_->rarity);
        if (!item_->name.empty()) ImGui::TextColored(color, "%s", item_->name.c_str());
        ImGui::TextColored(color, "%s", item_->base.c_str());
        ImGui::TextDisabled("%s | %s", item_->item_class.c_str(), item_->rarity.c_str());
        if (reply_ && reply_->status == "trade") {
            // The filters list the mods; no need for the raw text too.
            ImGui::Separator();
            draw_trade();
        } else {
            for (const auto& section : item_->sections) {
                ImGui::Separator();
                for (const auto& line : section) ImGui::TextUnformatted(line.c_str());
            }
            ImGui::Separator();
            draw_prices();
        }
    }
    if (!status_.empty()) ImGui::TextDisabled("%s", status_.c_str());
    ImGui::TextDisabled("Esc closes");

    ImGui::PopTextWrapPos();
    ImGui::End();
}

void PriceCheck::draw_prices() {
    if (!reply_) {
        ImGui::TextDisabled(daemon_.busy() ? "Checking price..." : "No price");
        return;
    }
    if (reply_->status != "ok") {
        ImGui::TextDisabled("%s", reply_->message.empty() ? reply_->status.c_str() : reply_->message.c_str());
    }
    for (const PriceLine& p : reply_->prices) {
        char value[64];
        if (p.divine >= 1.0)
            snprintf(value, sizeof(value), "%.1f div  (%.0fc)", p.divine, p.chaos);
        else
            snprintf(value, sizeof(value), p.chaos < 10 ? "%.1fc" : "%.0fc", p.chaos);

        if (!p.label.empty()) {
            ImGui::TextUnformatted(p.label.c_str());
            ImGui::SameLine();
        }
        ImGui::TextColored(ImVec4(1.0f, 0.85f, 0.4f, 1), "%s", value);
        if (!p.listings.empty()) {
            ImGui::SameLine();
            // Few listings: poe.ninja's number is a guess at best.
            bool thin = atoi(p.listings.c_str()) < 5;
            ImGui::TextColored(thin ? ImVec4(1, 0.5f, 0.4f, 1) : ImVec4(0.6f, 0.6f, 0.6f, 1), "%s listed",
                               p.listings.c_str());
        }
    }
    if (!reply_->league.empty())
        ImGui::TextDisabled("poe.ninja | %s | %d min old", reply_->league.c_str(), reply_->age_minutes);
}

void PriceCheck::draw_trade() {
    bool changed = false;
    for (TradeFilter& f : reply_->filters) {
        ImGui::PushID(f.index);
        changed |= ImGui::Checkbox(f.label.c_str(), &f.enabled);
        ImGui::PopID();
    }
    if (changed) {
        std::vector<int> enabled;
        for (const TradeFilter& f : reply_->filters)
            if (f.enabled) enabled.push_back(f.index);
        reply_->message.clear();
        daemon_.request_trade(enabled, item_text_);
    }

    ImGui::Separator();
    if (daemon_.busy()) ImGui::TextDisabled("Searching...");
    if (!reply_->message.empty()) ImGui::TextColored(ImVec4(1, 0.5f, 0.4f, 1), "%s", reply_->message.c_str());

    if (reply_->listings.empty()) {
        ImGui::TextDisabled("No listings");
    } else if (ImGui::BeginTable("listings", 2, ImGuiTableFlags_SizingFixedFit)) {
        for (const TradeListing& l : reply_->listings) {
            ImGui::TableNextRow();
            ImGui::TableNextColumn();
            ImGui::TextColored(ImVec4(1.0f, 0.85f, 0.4f, 1), "%s %s", l.amount.c_str(), l.currency.c_str());
            if (l.chaos >= 0 && l.currency != "Chaos Orb") {
                ImGui::SameLine();
                ImGui::TextDisabled("(%.0fc)", l.chaos);
            }
            ImGui::TableNextColumn();
            ImGui::TextDisabled("%s", l.seller.c_str());
        }
        ImGui::EndTable();
    }

    ImGui::TextDisabled("%s matches online | %s", reply_->total.c_str(), reply_->league.c_str());
    if (!reply_->url.empty() && ImGui::Button("Open on trade site")) opener_.open_url(reply_->url);
}
