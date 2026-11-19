// Talks to poe-overlay-daemon over the abstract socket @poe-overlay without
// ever blocking the frame: the request is written and the reply read a bit
// at a time from poll(), once per frame. Protocol: see daemon/src/main.rs.
#pragma once

#include <chrono>
#include <string>
#include <vector>

struct PriceLine {
    std::string label;
    double chaos = 0;
    double divine = 0;
    std::string listings;  // empty if unknown
};

struct TradeFilter {
    int index = 0;
    bool enabled = false;
    std::string label;
    std::string min;  // empty if none
};

struct TradeListing {
    std::string amount;
    std::string currency;
    double chaos = -1;  // < 0: unknown
    std::string age;    // "2m", "3h", "5d"; empty if unknown
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
    // status "trade"
    std::vector<TradeFilter> filters;
    std::vector<TradeListing> listings;
    std::string total;
    std::string url;
    // status "map"
    std::string subtitle;
    std::vector<MapStat> map_stats;
    std::vector<MapMod> map_mods;
    int profile = 1;
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
    // Any request body (see daemon/src/main.rs), for one-off messages.
    void send_raw(const std::string& body) { start(body); }
    // Rerun a trade search with only the filters at these indices.
    void request_trade(const std::vector<int>& enabled, const std::string& item_text);
    // Ask the daemon to open a trade search URL in the user's browser.
    void open_url(const std::string& url) { start("OPEN\n" + url); }
    bool busy() const { return fd_ >= 0; }

    // Advances I/O. Returns true exactly once per request, when *out holds
    // the reply (or a status "error" describing what went wrong).
    bool poll(PriceReply* out);

private:
    void start(const std::string& body);
    bool fail(PriceReply* out, const std::string& message);
    void close_socket();

    int fd_ = -1;
    std::string out_buf_;
    size_t out_sent_ = 0;
    std::string in_buf_;
    std::chrono::steady_clock::time_point deadline_;
};
