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

struct PriceReply {
    std::string status;  // ok, not_found, unsupported, loading, error, ...
    std::string message;
    std::string title;
    std::string league;
    int age_minutes = -1;
    std::vector<PriceLine> prices;
};

class DaemonClient {
public:
    ~DaemonClient() { close_socket(); }

    // Starts a lookup, dropping any request still in flight.
    void request(const std::string& item_text);
    bool busy() const { return fd_ >= 0; }

    // Advances I/O. Returns true exactly once per request, when *out holds
    // the reply (or a status "error" describing what went wrong).
    bool poll(PriceReply* out);

private:
    bool fail(PriceReply* out, const std::string& message);
    void close_socket();

    int fd_ = -1;
    std::string out_buf_;
    size_t out_sent_ = 0;
    std::string in_buf_;
    std::chrono::steady_clock::time_point deadline_;
};
