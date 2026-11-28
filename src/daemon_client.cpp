#include "daemon_client.h"
#include "game.h"

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>

namespace {

// The game's daemon. POE_WAYTRADE_SOCKET picks another name, to test
// against a separate daemon.
std::string socket_name() {
    const char* name = getenv("POE_WAYTRADE_SOCKET");
    return name && *name ? name : game_socket_name();
}
// A trade search can take a few seconds (browser start, two API requests).
constexpr auto kTimeout = std::chrono::seconds(20);
// ICON replies carry a PNG as base64: the socket sprite sheet is ~85 KB, and
// the daemon fetches images up to 1 MiB.
constexpr size_t kMaxReply = 2 * 1024 * 1024;

std::string frame(const std::string& body) {
    uint32_t len = (uint32_t)body.size();
    std::string out(reinterpret_cast<const char*>(&len), 4);  // little-endian on x86
    return out + body;
}

std::vector<std::string> split(const std::string& s, char sep) {
    std::vector<std::string> parts;
    size_t start = 0;
    for (;;) {
        size_t end = s.find(sep, start);
        parts.push_back(s.substr(start, end - start));
        if (end == std::string::npos) return parts;
        start = end + 1;
    }
}

// A price line's week: "<total change>;<day>,<day>,...", a day empty when
// it had no listings.
void parse_week(const std::string& field, PriceLine& p) {
    size_t semi = field.find(';');
    if (semi == std::string::npos) return;
    p.week_change = strtod(field.c_str(), nullptr);
    for (const std::string& day : split(field.substr(semi + 1), ','))
        p.week.push_back(day.empty() ? std::nullopt : std::optional<double>(strtod(day.c_str(), nullptr)));
}

// "<v>,<v>,,<v>": a value a day, none where empty.
std::vector<std::optional<double>> parse_days(const std::string& field) {
    std::vector<std::optional<double>> days;
    for (const std::string& day : split(field, ','))
        days.push_back(day.empty() ? std::nullopt : std::optional<double>(strtod(day.c_str(), nullptr)));
    return days;
}

PriceReply parse_reply(const std::string& body) {
    PriceReply reply;
    for (const std::string& line : split(body, '\n')) {
        std::vector<std::string> f = split(line, '\t');
        if (f.size() < 2) continue;
        if (f[0] == "status") reply.status = f[1];
        else if (f[0] == "message") reply.message = f[1];
        else if (f[0] == "title") reply.title = f[1];
        else if (f[0] == "history") reply.history.values = parse_days(f[1]);
        else if (f[0] == "counts") reply.history.counts = parse_days(f[1]);
        else if (f[0] == "league") reply.league = f[1];
        else if (f[0] == "age") reply.age_minutes = atoi(f[1].c_str());
        else if (f[0] == "price" && f.size() >= 5) {
            PriceLine p{f[1], strtod(f[2].c_str(), nullptr), strtod(f[3].c_str(), nullptr), f[4],
                        f.size() >= 6 ? f[5] : ""};
            if (f.size() >= 7) parse_week(f[6], p);
            if (f.size() >= 8) p.history_ref = f[7];
            reply.prices.push_back(std::move(p));
        }
        else if (f[0] == "filter" && f.size() >= 4) {
            TradeFilter t{atoi(f[1].c_str()), f[2] == "1", f[3], f.size() >= 5 ? f[4] : ""};
            if (f.size() >= 10) {
                t.lo = strtod(f[5].c_str(), nullptr);
                t.hi = strtod(f[6].c_str(), nullptr);
                t.step = strtod(f[7].c_str(), nullptr);
                t.max_roll = f[8] == "1";
                t.section = f[9];
            }
            if (f.size() >= 11) t.max = f[10];
            if (f.size() >= 12) t.tier = atoi(f[11].c_str());
            if (f.size() >= 13) t.roll = f[12];
            if (f.size() >= 14) t.column = f[13];
            if (f.size() >= 15) t.draw_slider = f[14] != "0";
            if (f.size() >= 16) t.increased = atoi(f[15].c_str());
            reply.filters.push_back(t);
        } else if (f[0] == "listing" && f.size() >= 5) {
            TradeListing l{f[1], f[2], f[3].empty() ? -1 : strtod(f[3].c_str(), nullptr), f[4]};
            if (f.size() >= 7) l.count = atoi(f[5].c_str()), l.currency_id = f[6];
            reply.listings.push_back(l);
        } else if (f[0] == "preview" && f.size() >= 3 && !reply.listings.empty()) {
            reply.listings.back().preview.push_back({f[1], f[2], f.size() >= 4 ? atoi(f[3].c_str()) : 0});
        } else if (f[0] == "value" && f.size() >= 3 && !reply.listings.empty()) {
            reply.listings.back().values.push_back({atoi(f[1].c_str()), f[2]});
        } else if (f[0] == "summary" && f.size() >= 6) {
            reply.summary = TradeSummary{f[1], f[2], f[3], f[5], f[4].empty() ? -1 : strtod(f[4].c_str(), nullptr)};
            reply.summary->capped = f.size() >= 7 && f[6] == "1";
        }
        else if (f[0] == "png") reply.png = f[1];
        else if (f[0] == "art" && f.size() >= 3) reply.art_width = atoi(f[1].c_str()), reply.art_height = atoi(f[2].c_str());
        else if (f[0] == "matched") reply.matched = f[1];
        else if (f[0] == "subtitle") reply.subtitle = f[1];
        else if (f[0] == "stat" && f.size() >= 3) reply.map_stats.push_back({f[1], f[2]});
        else if (f[0] == "mod" && f.size() >= 4) reply.map_mods.push_back({f[1], f[2], f[3]});
        else if (f[0] == "profile") reply.profile = atoi(f[1].c_str());
        else if (f[0] == "total") reply.total = f[1];
        else if (f[0] == "searched") reply.searched = f[1] == "1";
        else if (f[0] == "option" && f.size() >= 4 && f[1] == "base") reply.base_type = f[2], reply.category_label = f[3];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "base_only") reply.base_only = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "corrupted") reply.corrupted = f[2];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "item_corrupted") reply.item_corrupted = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "status") reply.listing_status = f[2];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "orbs_only") reply.orbs_only = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "base_preset") reply.base_preset = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "preset") reply.preset = f[2];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "six_link") reply.six_link_offered = true, reply.six_link = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "unidentified")
            reply.unidentified_offered = true, reply.unidentified = f[2] == "1";
        else if (f[0] == "url") reply.url = f[1];
        else if (f[0] == "setting" && f.size() >= 3) reply.settings.push_back({f[1], f[2]});
        else if (f[0] == "setting") reply.settings.push_back({f[1], ""});
        else if (f[0] == "league_option") reply.league_options.push_back(f[1]);
        else if (f[0] == "league_current" && f.size() >= 3) reply.league_current.push_back({f[1], f[2]});
        else if (f[0] == "league_pinned") reply.league_pinned = f[1];
        else if (f[0] == "map_profile") reply.profile = atoi(f[1].c_str());
        else if (f[0] == "version") reply.version = f[1];
        else if (f[0] == "log") reply.log = f[1];
        else if (f[0] == "plate" && f.size() >= 9) {
            std::array<float, 8> p;
            for (int i = 0; i < 8; i++) p[i] = strtof(f[i + 1].c_str(), nullptr);
            reply.plates.push_back(p);
        } else if (f[0] == "detail") reply.detail = f[1];
        else if (f[0] == "value" && f.size() >= 5) {
            reply.value_chaos = strtod(f[1].c_str(), nullptr);
            reply.value_divine = strtod(f[2].c_str(), nullptr);
            reply.value_text = f[3];
            reply.value_source = f[4];
        } else if (f[0] == "line") reply.lines.push_back(f[1]);
        else if (f[0] == "facts") reply.facts = atoi(f[1].c_str());
        else if (f[0] == "area") reply.area = f[1];
        else if (f[0] == "entered") reply.entered = atoll(f[1].c_str());
        else if (f[0] == "watch") reply.watch = f[1];
    }
    if (reply.title == "Divine Orb")
        for (PriceLine& p : reply.prices) p.divine_orb = true;
    return reply;
}

}  // namespace

std::string DaemonClient::trade_body(const std::vector<TradeFilter>& enabled, bool base_only,
                                     const std::string& corrupted, const std::string& preset,
                                     const std::string& listing_status, bool six_link, bool unidentified,
                                     bool orbs_only, const std::string& item_text) {
    std::string indices;
    for (const TradeFilter& f : enabled)
        indices += (indices.empty() ? "" : ",") + std::to_string(f.index) + ":" + f.min + ":" + f.max;
    return indices + "\n" + (base_only ? "base" : "category") + "\n" + corrupted + "\n" + preset + "\n" + listing_status +
           "\n" + (six_link ? "6" : "any") + "\n" + (unidentified ? "unid" : "any") + "\n" + (orbs_only ? "orbs" : "any") + "\n" + item_text;
}

void DaemonClient::start(const std::string& body) {
    close_socket();
    out_buf_ = frame(body);
    out_sent_ = 0;
    in_buf_.clear();
    deadline_ = std::chrono::steady_clock::now() + kTimeout;

    fd_ = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd_ < 0) return;  // poll() reports it

    // Abstract address: leading NUL, no terminator, length covers the name.
    sockaddr_un addr = {};
    addr.sun_family = AF_UNIX;
    std::string name = socket_name().substr(0, sizeof(addr.sun_path) - 1);
    memcpy(addr.sun_path + 1, name.data(), name.size());
    socklen_t len = offsetof(sockaddr_un, sun_path) + 1 + name.size();
    if (connect(fd_, reinterpret_cast<sockaddr*>(&addr), len) != 0 && errno != EAGAIN &&
        errno != EINPROGRESS) {
        close_socket();
    }
}

bool DaemonClient::poll(PriceReply* out) {
    if (out_buf_.empty()) return false;  // no request pending
    if (fd_ < 0) return fail(out, is_poe2() ? "Price daemon isn't running (start the game with waytrade %command%)"
                                   : "Price daemon isn't running (systemctl --user start poe-waytrade-daemon.socket)");
    if (std::chrono::steady_clock::now() > deadline_) return fail(out, "Price daemon didn't answer");

    while (out_sent_ < out_buf_.size()) {
        ssize_t n = send(fd_, out_buf_.data() + out_sent_, out_buf_.size() - out_sent_, MSG_NOSIGNAL);
        if (n < 0) {
            if (errno == EAGAIN || errno == EINTR) return false;
            return fail(out, std::string("Price daemon: ") + strerror(errno));
        }
        out_sent_ += n;
    }

    char chunk[4096];
    for (;;) {
        ssize_t n = recv(fd_, chunk, sizeof(chunk), 0);
        if (n > 0) {
            in_buf_.append(chunk, n);
            if (in_buf_.size() > kMaxReply + 4) return fail(out, "Price daemon reply too large");
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EINTR)) break;
        break;  // EOF or error: check below whether the reply is complete
    }

    if (in_buf_.size() < 4) return false;
    uint32_t len;
    memcpy(&len, in_buf_.data(), 4);
    if (in_buf_.size() < 4 + (size_t)len) return false;

    *out = parse_reply(in_buf_.substr(4, len));
    close_socket();
    out_buf_.clear();
    return true;
}

bool DaemonClient::fail(PriceReply* out, const std::string& message) {
    *out = PriceReply{};
    out->status = "error";
    out->message = message;
    close_socket();
    out_buf_.clear();
    return true;
}

void DaemonClient::close_socket() {
    if (fd_ >= 0) close(fd_);
    fd_ = -1;
}
