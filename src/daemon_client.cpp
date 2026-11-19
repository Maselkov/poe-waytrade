#include "daemon_client.h"

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>

namespace {

// POE_WAYTRADE_SOCKET picks another name, to test against a separate daemon.
std::string socket_name() {
    const char* name = getenv("POE_WAYTRADE_SOCKET");
    return name && *name ? name : "poe-waytrade";
}
// A trade search can take a few seconds (browser start, two API requests).
constexpr auto kTimeout = std::chrono::seconds(20);
constexpr size_t kMaxReply = 64 * 1024;

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

PriceReply parse_reply(const std::string& body) {
    PriceReply reply;
    for (const std::string& line : split(body, '\n')) {
        std::vector<std::string> f = split(line, '\t');
        if (f.size() < 2) continue;
        if (f[0] == "status") reply.status = f[1];
        else if (f[0] == "message") reply.message = f[1];
        else if (f[0] == "title") reply.title = f[1];
        else if (f[0] == "league") reply.league = f[1];
        else if (f[0] == "age") reply.age_minutes = atoi(f[1].c_str());
        else if (f[0] == "price" && f.size() >= 5)
            reply.prices.push_back({f[1], strtod(f[2].c_str(), nullptr), strtod(f[3].c_str(), nullptr), f[4]});
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
            reply.filters.push_back(t);
        } else if (f[0] == "listing" && f.size() >= 5) {
            TradeListing l{f[1], f[2], f[3].empty() ? -1 : strtod(f[3].c_str(), nullptr), f[4]};
            if (f.size() >= 7) l.count = atoi(f[5].c_str()), l.currency_id = f[6];
            reply.listings.push_back(l);
        } else if (f[0] == "preview" && f.size() >= 3 && !reply.listings.empty()) {
            reply.listings.back().preview.push_back({f[1], f[2], f.size() >= 4 ? atoi(f[3].c_str()) : 0});
        } else if (f[0] == "summary" && f.size() >= 6) {
            reply.summary = TradeSummary{f[1], f[2], f[3], f[5], f[4].empty() ? -1 : strtod(f[4].c_str(), nullptr)};
            reply.summary->capped = f.size() >= 7 && f[6] == "1";
        }
        else if (f[0] == "png") reply.png = f[1];
        else if (f[0] == "subtitle") reply.subtitle = f[1];
        else if (f[0] == "stat" && f.size() >= 3) reply.map_stats.push_back({f[1], f[2]});
        else if (f[0] == "mod" && f.size() >= 4) reply.map_mods.push_back({f[1], f[2], f[3]});
        else if (f[0] == "profile") reply.profile = atoi(f[1].c_str());
        else if (f[0] == "total") reply.total = f[1];
        else if (f[0] == "searched") reply.searched = f[1] == "1";
        else if (f[0] == "option" && f.size() >= 4 && f[1] == "base") reply.base_type = f[2], reply.category_label = f[3];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "base_only") reply.base_only = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "corrupted") reply.corrupted = f[2];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "status") reply.listing_status = f[2];
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "base_preset") reply.base_preset = f[2] == "1";
        else if (f[0] == "option" && f.size() >= 3 && f[1] == "preset") reply.preset = f[2];
        else if (f[0] == "url") reply.url = f[1];
    }
    return reply;
}

}  // namespace

void DaemonClient::request_trade(const std::vector<TradeFilter>& enabled, bool base_only,
                                 const std::string& corrupted, const std::string& preset,
                                 const std::string& listing_status, const std::string& item_text) {
    std::string indices;
    for (const TradeFilter& f : enabled)
        indices += (indices.empty() ? "" : ",") + std::to_string(f.index) + ":" + f.min + ":" + f.max;
    start("TRADE\n" + indices + "\n" + (base_only ? "base" : "category") + "\n" + corrupted + "\n" + preset + "\n" + listing_status +
          "\n" + item_text);
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
    if (fd_ < 0) return fail(out, "Price daemon isn't running (systemctl --user start poe-waytrade-daemon.socket)");
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
