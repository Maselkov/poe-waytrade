#include "settings_page.h"
#include "layer.h"
#include "wl_input.h"

#include <linux/input-event-codes.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>

namespace {

const char* const kHotkeyKeys[SettingsPage::kActions] = {
    "hotkey.quick",
    "hotkey.locked",
    "hotkey.map_alias",
    "hotkey.settings",
    "hotkey.displays",
};

int index(SettingsPage::Action a) { return (int)a; }

std::string scale_text(double scale) {
    char buf[16];
    snprintf(buf, sizeof buf, "%.2f", scale);
    return buf;
}

}  // namespace

std::array<KeyCombo, SettingsPage::kActions> SettingsPage::Values::defaults() {
    return {KeyCombo{kModCtrl, KEY_D}, KeyCombo{kModCtrl | kModAlt, KEY_D}, KeyCombo{kModAlt, KEY_D},
            KeyCombo{kModCtrl | kModAlt, KEY_S}, KeyCombo{kModCtrl | kModAlt, KEY_F}};
}

const char* SettingsPage::action_label(Action action) {
    switch (action) {
    case Action::Quick: return "Quick look";
    case Action::Locked: return "Locked panel";
    case Action::MapAlias: return "Locked panel, again";
    case Action::Settings: return "Open settings";
    case Action::Displays: return "Price displays";
    }
    return "";
}

SettingsPage::SettingsPage() {
    scale_from_env_ = getenv("POE_WAYTRADE_UI_SCALE") != nullptr;
    send("SETTINGS\n");
    // POE_WAYTRADE_DEMO_SETTINGS=<tab 0-3>: open on start, for design work.
    if (const char* tab = getenv("POE_WAYTRADE_DEMO_SETTINGS")) {
        tab_ = (Tab)std::clamp(atoi(tab), 0, 3);
        visible_ = true;
    }
}

void SettingsPage::send(const std::string& body) { outbox_.push_back(body); }

void SettingsPage::update(WaylandInput& input, int game_clicks) {
    PriceReply reply;
    if (client_.poll(&reply)) {
        if (reply.status == "ok") {
            apply_reply(reply);
            // A change is saved once its reply is in.
            if (in_flight_.rfind("SET\n", 0) == 0 && outbox_.empty() && !note_error_ && note_.empty()) set_note("Saved");
            if (in_flight_.rfind("MAPRESET\n", 0) == 0) set_note("Profile's marks are back to the defaults");
        } else if (in_flight_.rfind("SET\n", 0) == 0 || in_flight_.rfind("MAP", 0) == 0) {
            set_note(reply.message.empty() ? "Couldn't save: " + reply.status : reply.message, true);
            // Show what the daemon has, not what we hoped.
            if (outbox_.empty()) send("SETTINGS\n");
        } else if (!loaded_) {
            poe_log("settings: %s", reply.message.c_str());
        }
        in_flight_.clear();
    }
    if (!client_.busy() && in_flight_.empty() && !outbox_.empty()) {
        in_flight_ = outbox_.front();
        outbox_.pop_front();
        client_.send_raw(in_flight_);
    }

    if (bindings_dirty_) {
        input.set_bindings(bindings());
        bindings_dirty_ = false;
    }
    if (cancel_requested_) {
        input.cancel_capture();
        cancel_requested_ = false;
    }
    if (capture_requested_) {
        input.begin_capture();
        capture_requested_ = false;
    }
    if (capturing_ && !input.capturing()) {
        if (std::optional<KeyCombo> combo = input.take_captured()) {
            Action action = *capturing_;
            capturing_.reset();
            if (combo->code == KEY_ESC && combo->mods == 0) {
                set_note("");
            } else if (combo->code == KEY_C && (combo->mods & kModCtrl)) {
                set_note("Ctrl + C copies items for the price check: press another", true);
                begin_capture(action);
            } else if (!(combo->mods & (kModCtrl | kModAlt | kModSuper))) {
                warning_ = Warning{action, *combo};
            } else {
                assign(action, *combo);
            }
            changed();
        }
    }
    // A click in the game closes the page, like a locked panel. Not while
    // recording a key: that click is how you'd notice it went to the game.
    if (visible_ && game_clicks > 0 && !capturing_) close();
}

void SettingsPage::apply_reply(const PriceReply& reply) {
    Values v = values_;
    for (const auto& [key, value] : reply.settings) {
        if (key == "league") v.league = value;
        else if (key == "status") v.status = value;
        else if (key == "min_percent") v.min_percent = atoi(value.c_str());
        else if (key == "ui_scale") v.ui_scale = atof(value.c_str());
        else if (key == "stash_scroll") v.stash_scroll = value != "off";
        else if (key == "cursor") v.desktop_cursor = value == "desktop";
        else if (key == "auto_watch") v.auto_watch = value != "off";
        for (int i = 0; i < kActions; i++)
            if (key == kHotkeyKeys[i]) v.hotkeys[i] = parse_combo(value);
    }
    if (reply.settings.empty()) return;  // not a settings reply
    if (v.hotkeys != values_.hotkeys) bindings_dirty_ = true;
    values_ = v;
    leagues_ = reply.league_options;
    league_current_ = reply.league_current;
    league_pinned_ = reply.league_pinned;
    league_active_ = reply.league;
    daemon_version_ = reply.version;
    daemon_log_ = reply.log;
    map_profile_ = reply.profile;
    if (!loaded_) poe_log("settings: loaded from the daemon");
    loaded_ = true;
    changed();
}

std::vector<HotkeyBinding> SettingsPage::bindings() const {
    const Hotkey hotkeys[kActions] = {Hotkey::PriceCheck, Hotkey::PriceCheckLocked, Hotkey::MapCheck,
                                      Hotkey::Settings, Hotkey::Displays};
    std::vector<HotkeyBinding> out;
    for (int i = 0; i < kActions; i++)
        if (!values_.hotkeys[i].empty()) out.push_back({values_.hotkeys[i], hotkeys[i]});
    return out;
}

void SettingsPage::set_note(std::string note, bool error) {
    note_ = std::move(note);
    note_error_ = error;
    changed();
}

void SettingsPage::open() {
    if (visible_) return;
    visible_ = true;
    if (outbox_.empty() && in_flight_.empty()) send("SETTINGS\n");  // the league list may be new
    set_note("");
    changed();
}

void SettingsPage::close() {
    if (!visible_) return;
    visible_ = false;
    cancel_capture();
    warning_.reset();
    reset_armed_ = false;
    changed();
}

void SettingsPage::set_tab(Tab tab) {
    cancel_capture();
    warning_.reset();
    reset_armed_ = false;
    tab_ = tab;
    set_note("");
}

// ---- hotkeys ----------------------------------------------------------------

void SettingsPage::begin_capture(Action action) {
    warning_.reset();
    poe_log("settings: press the new keys for %s", action_label(action));
    capturing_ = action;
    capture_requested_ = true;
    changed();
}

void SettingsPage::cancel_capture() {
    if (!capturing_) return;
    capturing_.reset();
    capture_requested_ = false;
    cancel_requested_ = true;
    changed();
}

void SettingsPage::warning_retry() {
    if (warning_) begin_capture(warning_->action);
}

void SettingsPage::warning_accept() {
    if (!warning_) return;
    Warning w = *warning_;
    warning_.reset();
    assign(w.action, w.combo);
}

void SettingsPage::warning_keep() {
    warning_.reset();
    changed();
}

// A combination another action has moves that action to this one's old
// combination: the two swap, and the note says so.
void SettingsPage::assign(Action action, KeyCombo combo) {
    int i = index(action);
    KeyCombo old = values_.hotkeys[i];
    if (old == combo) {
        set_note("");
        return;
    }
    std::string body = "SET\n" + std::string(kHotkeyKeys[i]) + "\t" + format_combo(combo);
    std::string note;
    for (int j = 0; j < kActions; j++) {
        if (j == i || values_.hotkeys[j] != combo) continue;
        // Only some actions may be left without a key.
        KeyCombo moved = old;
        if (moved.empty() && !can_be_off((Action)j)) continue;
        values_.hotkeys[j] = moved;
        body += "\n" + std::string(kHotkeyKeys[j]) + "\t" + format_combo(moved);
        note = std::string(action_label((Action)j)) + (moved.empty() ? " is off now" : " moved to " + combo_label(moved));
    }
    values_.hotkeys[i] = combo;
    bindings_dirty_ = true;
    send(body);
    set_note(note);
}

void SettingsPage::clear_hotkey(Action action) {
    if (!can_be_off(action)) return;
    cancel_capture();
    values_.hotkeys[index(action)] = {};
    bindings_dirty_ = true;
    set(kHotkeyKeys[index(action)], "");
    set_note("");
}

void SettingsPage::set_stash_scroll(bool on) {
    if (on == values_.stash_scroll) return;
    values_.stash_scroll = on;
    set("stash_scroll", on ? "on" : "off");
    set_note("");
}

void SettingsPage::set_auto_watch(bool on) {
    if (on == values_.auto_watch) return;
    values_.auto_watch = on;
    set("auto_watch", on ? "on" : "off");
    set_note("");
}

void SettingsPage::set_desktop_cursor(bool on) {
    if (on == values_.desktop_cursor) return;
    values_.desktop_cursor = on;
    set("cursor", on ? "desktop" : "game");
    set_note("");
}

// ---- price check -------------------------------------------------------------

std::vector<SettingsPage::LeagueChoice> SettingsPage::league_choices() const {
    std::vector<LeagueChoice> out = {{"auto", "Current", ""}, {"auto-hardcore", "Current hardcore", ""}};
    for (auto& c : out)
        for (const auto& [setting, league] : league_current_)
            if (setting == c.value) c.now = league;
    for (const auto& name : leagues_) out.push_back({name, name, ""});
    auto saved = [&](const LeagueChoice& c) { return c.value == values_.league; };
    if (std::none_of(out.begin(), out.end(), saved)) out.push_back({values_.league, values_.league, ""});
    return out;
}

void SettingsPage::set_league(const std::string& league) {
    if (!league_pinned_.empty() || league == values_.league) return;
    values_.league = league;
    set("league", league);
    set_note("");
}

void SettingsPage::set_status(const std::string& status) {
    if (status == values_.status) return;
    values_.status = status;
    set("status", status);
    set_note("");
}

void SettingsPage::step_min(int delta) {
    int min = std::clamp(values_.min_percent + delta, 50, 100);
    if (min == values_.min_percent) return;
    values_.min_percent = min;
    set("min_percent", std::to_string(min));
    set_note("");
}

void SettingsPage::step_scale(int delta) {
    double scale = std::clamp(std::round(values_.ui_scale * 10 + delta) / 10, 0.5, 2.0);
    if (std::abs(scale - values_.ui_scale) < 1e-6) return;
    values_.ui_scale = scale;
    set("ui_scale", scale_text(scale));
    set_note("");
}

// ---- map check ---------------------------------------------------------------

void SettingsPage::set_map_profile(int profile) {
    if (profile < 1 || profile > 3 || profile == map_profile_) return;
    map_profile_ = profile;
    reset_armed_ = false;
    send("MAPPROFILE\n" + std::to_string(profile));
    set_note("");
}

void SettingsPage::reset_map_marks() {
    if (!reset_armed_) {
        reset_armed_ = true;
        changed();
        return;
    }
    reset_armed_ = false;
    send("MAPRESET\n" + std::to_string(map_profile_));
    set_note("");
}

void SettingsPage::reset_defaults() {
    cancel_capture();
    warning_.reset();
    Values defaults;
    if (defaults.hotkeys != values_.hotkeys) bindings_dirty_ = true;
    values_ = defaults;
    send("SET\nreset");
    set_note("");
}
