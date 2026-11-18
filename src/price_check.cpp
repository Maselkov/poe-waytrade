#include "price_check.h"
#include "layer.h"
#include "wl_input.h"

#include "imgui.h"

#include <linux/input-event-codes.h>

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
    ImGui::Begin("Price check", nullptr,
                 ImGuiWindowFlags_AlwaysAutoResize | ImGuiWindowFlags_NoCollapse |
                     ImGuiWindowFlags_NoSavedSettings | ImGuiWindowFlags_NoNav);
    ImGui::PushTextWrapPos(ImGui::GetFontSize() * 28);

    if (item_) {
        ImVec4 color = rarity_color(item_->rarity);
        if (!item_->name.empty()) ImGui::TextColored(color, "%s", item_->name.c_str());
        ImGui::TextColored(color, "%s", item_->base.c_str());
        ImGui::TextDisabled("%s | %s", item_->item_class.c_str(), item_->rarity.c_str());
        for (const auto& section : item_->sections) {
            ImGui::Separator();
            for (const auto& line : section) ImGui::TextUnformatted(line.c_str());
        }
        ImGui::Separator();
        ImGui::TextDisabled("Prices: not implemented yet");
    }
    if (!status_.empty()) ImGui::TextDisabled("%s", status_.c_str());
    ImGui::TextDisabled("Esc closes");

    ImGui::PopTextWrapPos();
    ImGui::End();
}
