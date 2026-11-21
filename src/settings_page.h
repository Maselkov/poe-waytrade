// The settings page: hotkeys, panel size, item displays, league, search
// defaults, map check profile. The daemon keeps the settings (daemon/src/settings.rs) and
// saves them; we fetch them at start, show them, and send each change. Until
// the daemon answers, the defaults below apply, so the hotkeys work at once.
#pragma once

#include "daemon_client.h"
#include "hotkeys.h"
#include "wl_input.h"

#include <array>
#include <chrono>
#include <cstdint>
#include <deque>
#include <optional>
#include <string>
#include <vector>


class SettingsPage {
public:
    using Clock = std::chrono::steady_clock;

    enum class Tab { Hotkeys, General, Price, Map, About };
    // The rebindable hotkeys, in the order shown.
    enum class Action { Quick, Locked, MapAlias, Settings, Displays };
    static constexpr int kActions = 5;

    struct Values {
        std::string league = "auto";
        std::string status = "instant";  // instant | available | any
        int min_percent = 90;
        double ui_scale = 1.0;
        std::array<KeyCombo, kActions> hotkeys = defaults();
        bool stash_scroll = true;     // Ctrl + wheel switches stash tabs
        bool desktop_cursor = false;  // over the panels; else the game's
        bool auto_watch = true;       // price displays on entering a vault
        bool gpu_tracking = true;     // display labels placed by the GPU (gpu_track.h)
        bool heist_watch = true;      // watch for curio displays in Heist
        bool startup_notice = true;   // the "Waytrade is running" toast
        bool copy_check = false;      // the user's own Ctrl+C opens a quick look
        static std::array<KeyCombo, kActions> defaults();
    };

    SettingsPage();

    // Once per frame: daemon replies, captured keys, and the hotkey bindings
    // handed to `input`. `game_clicks`: clicks on the game, which close the
    // page like a locked panel.
    void update(WaylandInput& input, int game_clicks);
    uint64_t revision() const { return revision_; }

    bool visible() const { return visible_; }
    void open();
    void close();
    void toggle() { visible_ ? close() : open(); }

    Tab tab() const { return tab_; }
    void set_tab(Tab tab);

    const Values& values() const { return values_; }
    // From the daemon (empty until it answers).
    // The league picker's choices, in order: the two that follow poe.ninja,
    // then its leagues (and the saved one, if poe.ninja no longer has it).
    struct LeagueChoice {
        std::string value;  // the setting: "auto", "auto-hardcore" or a name
        std::string label;
        std::string now;  // for the "auto" ones: what they pick now
    };
    std::vector<LeagueChoice> league_choices() const;
    const std::string& league_pinned() const { return league_pinned_; }
    const std::string& league_active() const { return league_active_; }
    const std::string& daemon_version() const { return daemon_version_; }
    const std::string& daemon_log() const { return daemon_log_; }
    bool loaded() const { return loaded_; }
    int map_profile() const { return map_profile_; }
    // A short line under the page: saved, swapped, errors.
    const std::string& note() const { return note_; }
    bool note_error() const { return note_error_; }

    // Hotkeys: click a key button, then press the new combination. A key
    // without Ctrl or Alt reaches the game too: warning() asks first.
    void begin_capture(Action action);
    void cancel_capture();
    std::optional<Action> capturing() const { return capturing_; }
    struct Warning {
        Action action;
        KeyCombo combo;
    };
    const std::optional<Warning>& warning() const { return warning_; }
    void warning_retry();   // press another
    void warning_accept();  // use it anyway
    void warning_keep();    // keep the old one
    void clear_hotkey(Action action);  // only MapAlias and Displays can be off
    static bool can_be_off(Action action) { return action == Action::MapAlias || action == Action::Displays; }
    static const char* action_label(Action action);
    void set_stash_scroll(bool on);

    // General
    void step_scale(int delta);  // tenths
    bool scale_from_env() const { return scale_from_env_; }
    void set_desktop_cursor(bool on);
    void set_auto_watch(bool on);
    void set_gpu_tracking(bool on);
    void set_heist_watch(bool on);
    void set_startup_notice(bool on);
    void set_copy_check(bool on);

    // Price check
    void set_league(const std::string& league);
    void set_status(const std::string& status);
    void step_min(int delta);  // percent

    // Map check
    void set_map_profile(int profile);
    // Two clicks: the first asks, the second resets.
    void reset_map_marks();
    bool reset_armed() const { return reset_armed_; }

    void reset_defaults();

private:
    void changed() { revision_++; }
    void send(const std::string& body);
    void set(const std::string& key, const std::string& value) { send("SET\n" + key + "\t" + value); }
    void apply_reply(const PriceReply& reply);
    void assign(Action action, KeyCombo combo);
    void set_note(std::string note, bool error = false);
    std::vector<HotkeyBinding> bindings() const;

    uint64_t revision_ = 0;
    bool visible_ = false;
    Tab tab_ = Tab::Hotkeys;
    Values values_;
    bool loaded_ = false;
    std::vector<std::string> leagues_;
    std::vector<std::pair<std::string, std::string>> league_current_;
    std::string league_pinned_, league_active_, daemon_version_, daemon_log_;
    int map_profile_ = 1;
    bool scale_from_env_ = false;
    std::string note_;
    bool note_error_ = false;
    std::optional<Action> capturing_;
    bool capture_requested_ = false;  // begin_capture on input next update
    bool cancel_requested_ = false;
    std::optional<Warning> warning_;
    bool reset_armed_ = false;

    // Requests go out one at a time, in order: SETTINGS first, then changes.
    DaemonClient client_;
    std::deque<std::string> outbox_;
    std::string in_flight_;
    bool bindings_dirty_ = true;
};
