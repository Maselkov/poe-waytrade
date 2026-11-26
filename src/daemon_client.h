// Talks to poe-waytrade-daemon over the abstract socket @poe-waytrade without
// ever blocking the frame: the request is written and the reply read a bit
// at a time from poll(), once per frame. Protocol: see daemon/src/main.rs.
#pragma once

#include <array>
#include <chrono>
#include <optional>
#include <string>
#include "game.h"

#include <utility>
#include <vector>

struct PriceLine {
    std::string label;
    double chaos = 0;
    double divine = 0;
    std::string listings;  // empty if unknown
    // On an unidentified unique's list of candidates: which unique this is,
    // so clicking it checks the item as that one (PriceCheck::pick_unique).
    std::string unique;
    // The price over the last week, a day at a time: each day's change in
    // percent against the week's start, none on a day with no listings.
    // Empty when poe.ninja has no days for it.
    std::vector<std::optional<double>> week;
    double week_change = 0;  // over the whole week, in percent
    // A Divine Orb's own price: "1.0 div" would say nothing, so it's shown
    // in the small unit whatever it's worth.
    bool divine_orb = false;
    // Where its whole history is, for HISTORY: "item:<type>:<id>" or
    // "exchange:<type>:<details>"; empty when poe.ninja has none.
    std::string history_ref;
};

// A price's history over the whole league (HISTORY): a day each, oldest
// first, the last today; values in the small unit, counts the listings
// (items only); none on a day without data.
struct PriceHistory {
    std::vector<std::optional<double>> values;
    std::vector<std::optional<double>> counts;
};

// The currency a poe.ninja price is shown in (and its icon): divine from 1
// up, else the game's small unit (chaos, PoE2's exalted). `chaos` is in
// that unit.
inline bool in_divines(const PriceLine& p) { return p.divine >= 1.0 && !p.divine_orb; }
inline const char* price_icon(const PriceLine& p) { return in_divines(p) ? "divine" : small_unit_id(); }

struct TradeFilter {
    int index = 0;
    bool enabled = false;
    std::string label;
    std::string min;      // empty if none
    double lo = 0, hi = 0;  // slider range; lo == hi: no slider
    double step = 1;
    bool max_roll = false;
    std::string section;  // weapon | total | mod
    std::string max;      // empty if none
    int tier = 0;         // affix tier, 1 best; 0: none
    std::string roll;     // the item's own value; empty if none
    std::string column;   // short name over the listings' values ("Ele res")
    bool draw_slider = true;  // else lo..hi only bounds the wheel's steps
    int increased = 0;        // % a catalyst raised the mod by; 0: none
};

struct TradeListing {
    std::string amount;
    std::string currency;
    double chaos = -1;  // < 0: unknown
    std::string age;    // "2m", "41h–5d"; empty if unknown
    int count = 1;      // listings at this price
    std::string currency_id;
    // The first listed item, as (kind, text) lines: icon (image URL), cells
    // ("2 2"), rarity, name, base, ilvl, sockets, influence, dps, enchant,
    // implicit, then mods (fractured, explicit, crafted, veiled, ...),
    // mirrored, corrupted, unidentified. See daemon/src/trade.rs Preview.
    struct PreviewLine {
        std::string kind, text;
        int tier = 0;  // affix tier, 1 best; 0: none
    };
    std::vector<PreviewLine> preview;
    // What the listing has of the search's enabled filters: (filter index,
    // "158" / "81%").
    std::vector<std::pair<int, std::string>> values;
};

// The median of the fetched listings.
struct TradeSummary {
    std::string amount, currency, currency_id, range;
    double chaos = -1;
    bool capped = false;  // set by an unsold listing: worth less than this
};

struct MapStat {
    std::string label, value;
};

struct MapMod {
    std::string key;    // what the daemon stores marks under
    std::string text;   // as on the item
    std::string marks;  // one of d/w/g/- per profile, e.g. "dw-"
};

struct PriceReply {
    std::string status;  // ok, trade, not_found, unsupported, loading, error, ...
    std::string message;
    std::string title;
    std::string league;
    int age_minutes = -1;
    std::vector<PriceLine> prices;
    // An unidentified unique whose base has uniques with different art: its
    // size in cells, for MATCHART (0: nothing to tell apart).
    int art_width = 0, art_height = 0;
    // A MATCHART reply: the unique(s) the art matched ("A / B" if they share it).
    std::string matched;
    // status "trade"
    std::vector<TradeFilter> filters;
    std::vector<TradeListing> listings;
    std::string total;
    std::string url;
    std::optional<TradeSummary> summary;
    std::string png;  // ICON reply, base64
    PriceHistory history;  // HISTORY reply
    // Trade search options (see daemon/src/main.rs).
    std::string base_type;       // e.g. "Sapphire Ring"
    std::string category_label;  // e.g. "Ring"; empty: no base/category choice
    bool base_only = false;
    std::string corrupted = "any";  // any | no | yes
    bool item_corrupted = false;    // the panel toggles between its state and any
    // Which listings: instant (buyout, merchant tabs) | available (+ in person) | any
    std::string listing_status = "instant";
    bool base_preset = false;       // the "Base item" preset is offered
    std::string preset = "normal";  // normal | base
    bool six_link_offered = false;  // a 6-link: the panel shows the 6-link chip
    bool six_link = false;          // search 6-linked items only
    bool unidentified_offered = false;  // an unidentified item: the panel shows its chip
    bool unidentified = false;          // search unidentified items only
    bool searched = false;
    // status "map"
    std::string subtitle;
    std::vector<MapStat> map_stats;
    std::vector<MapMod> map_mods;
    int profile = 1;  // also SETTINGS' map_profile
    // SETTINGS / SET (see daemon/src/settings.rs): key, value
    std::vector<std::pair<std::string, std::string>> settings;
    std::vector<std::string> league_options;
    // What "auto" / "auto-hardcore" pick now: setting, league
    std::vector<std::pair<std::string, std::string>> league_current;
    std::string league_pinned;  // POE_LEAGUE overrides the setting
    std::string version;        // the daemon's
    std::string log;            // where the daemon logs
    // SCANFIND: title plates, each x y w h then its crop's x y w h (full-size pixels)
    std::vector<std::array<float, 8>> plates;
    // SCANITEM
    std::string detail;        // what else was read ("level 4, 23%")
    double value_chaos = -1;   // < 0: no price
    double value_divine = -1;
    std::string value_text;    // "3.2 div"
    std::string value_source;  // ninja | trade
    std::vector<std::string> lines;  // what was read, for the log
    int facts = 0;                   // how much of the price was read (SCANITEM)
    // AREA
    std::string area;
    long long entered = -1;  // a new number: a new area
    // How to watch this area for displays: "1" (a vault: all the time),
    // "idle" (Heist: slowly, at full speed while one's in view), "0" (not).
    std::string watch = "0";
};

class DaemonClient {
public:
    ~DaemonClient() { close_socket(); }

    // Start a request, dropping any still in flight: a price lookup, or a
    // ping that just makes systemd start the daemon so prices are loaded by
    // the first lookup.
    void request(const std::string& item_text) { start("PRICE\n" + item_text); }
    void ping() { start("PING\n"); }
    void request_map(const std::string& item_text) { start("MAPCHECK\n" + item_text); }
    // Which unique an unidentified one is, from the pixels around it:
    // "<w> <h> <cursor x> <cursor y> <cell>\n" + base64 RGB (see
    // daemon/src/main.rs).
    void request_art_match(const std::string& header, const std::string& pixels, const std::string& item_text) {
        start("MATCHART\n" + header + "\n" + pixels + "\n" + item_text);
    }
    // Any request body (see daemon/src/main.rs), for one-off messages.
    void send_raw(const std::string& body) { start(body); }
    // Search the trade site with the filters at these indices.
    // Each enabled filter by index, with its minimum ("" for the default).
    // `enabled`: the filters to search with, by index, with their min/max.
    void request_trade(const std::vector<TradeFilter>& enabled, bool base_only,
                       const std::string& corrupted, const std::string& preset,
                       const std::string& listing_status, bool six_link, bool unidentified,
                       const std::string& item_text) {
        start("TRADE\n" + trade_body(enabled, base_only, corrupted, preset, listing_status, six_link, unidentified,
                                     item_text));
    }
    // Open the same search on the trade site in the user's browser, without
    // searching.
    void open_trade(const std::vector<TradeFilter>& enabled, bool base_only, const std::string& corrupted,
                    const std::string& preset, const std::string& listing_status, bool six_link, bool unidentified,
                    const std::string& item_text) {
        start("TRADEOPEN\n" + trade_body(enabled, base_only, corrupted, preset, listing_status, six_link,
                                         unidentified, item_text));
    }
    // Trade filters for a preset (normal | base), without searching.
    void request_prepare(const std::string& preset, const std::string& item_text) {
        start("PREPARE\n" + preset + "\n" + item_text);
    }
    bool busy() const { return fd_ >= 0; }
    // Drops the request in flight, if any: its reply is never reported.
    void cancel() {
        close_socket();
        out_buf_.clear();
    }

    // Advances I/O. Returns true exactly once per request, when *out holds
    // the reply (or a status "error" describing what went wrong).
    bool poll(PriceReply* out);

private:
    static std::string trade_body(const std::vector<TradeFilter>& enabled, bool base_only,
                                  const std::string& corrupted, const std::string& preset,
                                  const std::string& listing_status, bool six_link, bool unidentified,
                                  const std::string& item_text);
    void start(const std::string& body);
    bool fail(PriceReply* out, const std::string& message);
    void close_socket();

    int fd_ = -1;
    std::string out_buf_;
    size_t out_sent_ = 0;
    std::string in_buf_;
    std::chrono::steady_clock::time_point deadline_;
};
