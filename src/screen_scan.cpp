#include "screen_scan.h"
#include "icons.h"
#include "layer.h"

#include <algorithm>
#include <climits>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <optional>
#include <thread>
#include <cmath>
#include <cstdlib>
#include <cstring>

namespace {

// Between SCANFINDs: often enough to catch displays coming into view.
constexpr auto kFindGap = std::chrono::milliseconds(300);
// After the daemon couldn't take a frame (or none could be copied).
constexpr auto kFindRetry = std::chrono::seconds(1);
constexpr auto kAreaGap = std::chrono::seconds(1);
// Turned on by hand where the area isn't known: off after this long
// without a display in sight.
constexpr auto kIdleOff = std::chrono::minutes(3);
// A tooltip not found for this long is forgotten (SCANFIND finds it again
// if it comes back; its price is remembered, see kRemember).
constexpr auto kForget = std::chrono::seconds(4);
// Its label hides after this many copies without finding it...
constexpr int kHideAfter = 8;
// ...and after this many it isn't looked for each frame any more (a wide
// search is costly): SCANFIND finds it again, a few times a second.
constexpr int kStopLooking = 3;
// The daemon is still loading prices or names: ask again after this.
constexpr auto kRetryLoading = std::chrono::seconds(2);
// Tracking: how well the plate must match where it's looked for.
constexpr float kTrackScore = 0.6f;
// A display seen again within this long is recognised by its name's
// pixels and shows its price at once; it's read again all the same, since
// name bands look much alike (the frame around the name is the same on
// every display), and a wrong guess is put right a moment later.
constexpr auto kRemember = std::chrono::minutes(10);
constexpr size_t kRememberMax = 64;
// How alike the name's pixels must be (the panel behind is see-through,
// so they never match exactly).
constexpr float kSameName = 0.9f;
// The name's band under the plate, in plate heights: one or two title lines.
constexpr float kNameBand = 1.4f;
// Motion: measured over about this long, and the furthest ahead a label is
// drawn from its last sample.
constexpr auto kVelocitySpan = std::chrono::milliseconds(100);
constexpr auto kVelocityMin = std::chrono::milliseconds(30);
constexpr float kMaxPredict = 0.1f;  // seconds

// POE_WAYTRADE_TRACK_LOG=1: log every tracking step, to check tracking.
bool track_log() {
    static const bool on = getenv("POE_WAYTRADE_TRACK_LOG") != nullptr;
    return on;
}

// Grey from RGB (Rec. 601).
inline uint8_t luma(const uint8_t* p) { return (uint8_t)((p[0] * 299 + p[1] * 587 + p[2] * 114) / 1000); }

// A grey image box-filtered down by `k`.
std::vector<uint8_t> shrink(const uint8_t* src, int w, int h, int k, int* ow, int* oh) {
    *ow = w / k;
    *oh = h / k;
    std::vector<uint8_t> out((size_t)*ow * *oh);
    for (int y = 0; y < *oh; y++)
        for (int x = 0; x < *ow; x++) {
            int sum = 0;
            for (int dy = 0; dy < k; dy++)
                for (int dx = 0; dx < k; dx++) sum += src[(size_t)(y * k + dy) * w + x * k + dx];
            out[(size_t)y * *ow + x] = (uint8_t)(sum / (k * k));
        }
    return out;
}

// Normalized cross-correlation of a template with the image at (x, y).
float ncc(const uint8_t* img, int iw, const std::vector<float>& t, float norm, int tw, int th, int x, int y) {
    double sum = 0, sq = 0, dot = 0;
    for (int ty = 0; ty < th; ty++) {
        const uint8_t* row = img + (size_t)(y + ty) * iw + x;
        const float* trow = t.data() + (size_t)ty * tw;
        for (int tx = 0; tx < tw; tx++) {
            double v = row[tx];
            sum += v;
            sq += v * v;
            dot += v * trow[tx];
        }
    }
    double n = (double)tw * th;
    double var = sq - sum * sum / n;
    if (var < 1 || norm < 1e-3f) return 0;
    return (float)(dot / (std::sqrt(var) * norm));
}

// Where the game's log probably is: the game runs in its install folder.
std::string guess_log() {
    char buf[PATH_MAX];
    if (!realpath("/proc/self/cwd", buf)) return "";
    return std::string(buf) + "/logs/Client.txt";
}

// "3.2" / "45" / "0.5": the label's amount.
std::string amount_text(double v) {
    char buf[32];
    if (v >= 10)
        snprintf(buf, sizeof buf, "%.0f", v);
    else
        snprintf(buf, sizeof buf, "%.1f", std::round(v * 10) / 10);
    std::string s = buf;
    if (s.size() > 2 && s.compare(s.size() - 2, 2, ".0") == 0) s.resize(s.size() - 2);
    return s;
}

}  // namespace

// ---- tracking thread ---------------------------------------------------------

namespace {

// Where a plate is in a region (grey, full size): coarse at quarter size
// everywhere, then at half size around the best, then to the pixel.
ScreenScan::Match match_plate(const ScreenScan::Plate& p, const ScreenScan::Region& r, const uint8_t* g1) {
    ScreenScan::Match m;
    int w4, h4, w2, h2;
    std::vector<uint8_t> g4 = shrink(g1, r.w, r.h, 4, &w4, &h4);
    float best = -2;
    int bx = 0, by = 0;
    for (int y = 0; y + p.t4.h <= h4; y++)
        for (int x = 0; x + p.t4.w <= w4; x++) {
            float s = ncc(g4.data(), w4, p.t4.v, p.t4.norm, p.t4.w, p.t4.h, x, y);
            if (s > best) best = s, bx = x, by = y;
        }
    std::vector<uint8_t> g2 = shrink(g1, r.w, r.h, 2, &w2, &h2);
    float half = -2;
    int hx = bx * 2, hy = by * 2;
    for (int y = std::max(0, by * 2 - 2); y <= std::min(h2 - p.t2.h, by * 2 + 2); y++)
        for (int x = std::max(0, bx * 2 - 2); x <= std::min(w2 - p.t2.w, bx * 2 + 2); x++) {
            float s = ncc(g2.data(), w2, p.t2.v, p.t2.norm, p.t2.w, p.t2.h, x, y);
            if (s > half) half = s, hx = x, hy = y;
        }
    m.score = half;
    if (half < kTrackScore) return m;
    float full = -2;
    int fx = hx * 2, fy = hy * 2;
    for (int y = std::max(0, hy * 2 - 1); y <= std::min(r.h - p.t1.h, hy * 2 + 1); y++)
        for (int x = std::max(0, hx * 2 - 1); x <= std::min(r.w - p.t1.w, hx * 2 + 1); x++) {
            float s = ncc(g1, r.w, p.t1.v, p.t1.norm, p.t1.w, p.t1.h, x, y);
            if (s > full) full = s, fx = x, fy = y;
        }
    m.found = true;
    m.score = full;
    m.x = (float)(r.x + fx);
    m.y = (float)(r.y + fy);
    return m;
}

}  // namespace

struct ScreenScan::Worker {
    struct Job {
        std::vector<Region> regions;
        std::vector<std::shared_ptr<const Plate>> plates;
        std::vector<std::vector<uint8_t>> raw;
        ToRgb to_rgb;
        Clock::time_point at;
        uint64_t seq = 0;
    };

    std::mutex lock;
    std::condition_variable wake;
    std::optional<Job> job;  // only the newest: an older one is stale
    std::vector<Match> done;
    bool quit = false;
    std::thread thread{[this] { run(); }};

    ~Worker() {
        {
            std::lock_guard lk(lock);
            quit = true;
        }
        wake.notify_all();
        thread.join();
    }

    void run() {
        for (;;) {
            Job j;
            {
                std::unique_lock lk(lock);
                wake.wait(lk, [&] { return quit || job; });
                if (quit) return;
                j = std::move(*job);
                job.reset();
            }
            std::vector<Match> out;
            for (size_t i = 0; i < j.regions.size(); i++) {
                const Region& r = j.regions[i];
                size_t n = (size_t)r.w * r.h;
                if (!j.plates[i] || j.raw[i].size() != n * 4) continue;
                std::vector<uint8_t> rgb(n * 3), gray(n);
                if (!j.to_rgb(j.raw[i].data(), n, rgb.data())) continue;
                for (size_t k = 0; k < n; k++) gray[k] = luma(&rgb[k * 3]);
                Match m = match_plate(*j.plates[i], r, gray.data());
                m.id = r.id;
                m.seq = j.seq;
                m.at = j.at;
                out.push_back(m);
            }
            std::lock_guard lk(lock);
            done.insert(done.end(), out.begin(), out.end());
        }
    }
};

ScreenScan::ScreenScan() : worker_(std::make_unique<Worker>()), log_hint_(guess_log()) {}

ScreenScan::~ScreenScan() = default;

void ScreenScan::start(bool automatic, Clock::time_point now) {
    tracked_.clear();
    remembered_.clear();
    active_ = true;
    automatic_ = automatic;
    started_ = last_plate_ = now;
    next_find_ = now;
    poe_log("displays: on (%s)", automatic ? "auto-watch area" : "hotkey");
    changed();
}

void ScreenScan::stop(const char* why) {
    if (!active_) return;
    active_ = false;
    tracked_.clear();
    remembered_.clear();
    frame_.clear();
    frame_.shrink_to_fit();
    // A frame on its way is dropped (waits for its worker to finish), and
    // so is a SCANFIND for it: its plates would be cropped from nothing.
    frame_job_ = {};
    frame_pending_ = false;
    find_busy_ = false;
    find_.cancel();
    poe_log("displays: off (%s)", why);
    changed();
}

void ScreenScan::forget(Tracked& t) {
    if (!t.replied || t.name.v.empty()) return;
    remembered_.push_back({std::move(t.name), std::move(t.reply), now_});
    if (remembered_.size() > kRememberMax) remembered_.erase(remembered_.begin());
}

void ScreenScan::update(Clock::time_point now, int toggles) {
    now_ = now;
    for (int i = 0; i < toggles; i++) {
        if (active_) {
            stop("hotkey");
            suppressed_ = true;  // don't come straight back on in this area
        } else {
            suppressed_ = false;
            start(false, now);
        }
    }
    poll_area(now);
    if (!active_) return;
    if (!automatic_ && !area_known_ && now - last_plate_ > kIdleOff) stop("no displays for a while");
    if (frame_job_.valid() && frame_job_.wait_for(std::chrono::seconds(0)) == std::future_status::ready) {
        FrameJob job = frame_job_.get();
        frame_pending_ = false;
        if (job.request.empty()) {
            next_find_ = now + kFindRetry;
        } else {
            frame_w_ = job_w_;
            frame_h_ = job_h_;
            frame_at_ = job_at_;
            frame_ = std::move(job.rgb);
            find_.send_raw(job.request);
            find_busy_ = true;
        }
    }
    poll_find(now);

    // Places the tracking thread found.
    std::vector<Match> matches;
    {
        std::lock_guard lk(worker_->lock);
        matches.swap(worker_->done);
    }
    for (const Match& m : matches) apply(m);

    for (Tracked& t : tracked_) {
        if (t.request_job.valid() && t.request_job.wait_for(std::chrono::seconds(0)) == std::future_status::ready)
            t.request = t.request_job.get();
        if (t.client && !t.request.empty() && !t.client->busy() && (!t.replied || t.verify) && now >= t.retry_at)
            t.client->send_raw(t.request);
        PriceReply reply;
        if (t.client && t.client->poll(&reply)) {
            if (reply.status == "loading") {
                t.retry_at = now + kRetryLoading;
                continue;
            }
            std::string lines;
            for (const auto& l : reply.lines) lines += (lines.empty() ? "" : " | ") + l;
            poe_log("displays: %s %s (%s) %s%s%s [%s]", reply.status.c_str(), reply.title.c_str(),
                    reply.detail.c_str(), reply.value_text.c_str(), reply.message.empty() ? "" : ": ",
                    reply.message.c_str(), lines.c_str());
            reply.lines.clear();
            if (t.verify && reply.title != t.reply.title)
                poe_log("displays: not %s after all, but %s", t.reply.title.c_str(), reply.title.c_str());
            t.reply = std::move(reply);
            t.replied = true;
            t.verify = false;
            t.request.clear();
            t.request.shrink_to_fit();
            changed();
        }
    }
    std::erase_if(remembered_, [&](const Remembered& r) { return now - r.at > kRemember; });
    size_t before = tracked_.size();
    for (Tracked& t : tracked_)
        if (t.lost > 0 && now - t.seen > kForget) forget(t);
    std::erase_if(tracked_, [&](const Tracked& t) { return t.lost > 0 && now - t.seen > kForget; });
    if (tracked_.size() != before) changed();
}

void ScreenScan::poll_area(Clock::time_point now) {
    PriceReply reply;
    if (area_client_.poll(&reply)) {
        area_known_ = reply.status == "ok";
        if (area_known_) {
            bool new_area = entered_ >= 0 && reply.entered != entered_;
            entered_ = reply.entered;
            if (new_area) {
                // Displays belong to their area; a hand turned-off one too.
                suppressed_ = false;
                stop("left the area");
            }
            if (reply.watch && !suppressed_ && !active_)
                start(true, now);
            else if (!reply.watch && active_ && automatic_)
                stop("auto-watch is off");
        }
    }
    if (!area_client_.busy() && now >= next_area_) {
        next_area_ = now + kAreaGap;
        area_client_.send_raw("AREA\n" + log_hint_);
    }
}

bool ScreenScan::frame_wanted(Clock::time_point now) const {
    return active_ && !frame_pending_ && !find_busy_ && now >= next_find_;
}

void ScreenScan::frame_taken(int width, int height, const uint8_t* px, ToRgb to_rgb, Clock::time_point at) {
    if (!active_ || !px || width < 4 || height < 4) {
        frame_pending_ = false;
        next_find_ = now_ + kFindRetry;
        return;
    }
    job_w_ = width;
    job_h_ = height;
    job_at_ = at;
    // frame_pending_ stays set until the job is done (see update()).
    frame_job_ = std::async(std::launch::async, [px, to_rgb, width, height]() {
        FrameJob job;
        job.rgb.resize((size_t)width * height * 3);
        if (!to_rgb(px, (size_t)width * height, job.rgb.data())) return FrameJob{};
        // Half size, grey: what the daemon looks for plates in.
        int hw = width / 2, hh = height / 2;
        std::vector<uint8_t> half((size_t)hw * hh);
        for (int y = 0; y < hh; y++)
            for (int x = 0; x < hw; x++) {
                const uint8_t* p = &job.rgb[((size_t)(y * 2) * width + x * 2) * 3];
                const uint8_t* q = p + (size_t)width * 3;
                half[(size_t)y * hw + x] = (uint8_t)((luma(p) + luma(p + 3) + luma(q) + luma(q + 3)) / 4);
            }
        char header[64];
        snprintf(header, sizeof header, "%d %d %d %d", hw, hh, width, height);
        job.request = "SCANFIND\n" + std::string(header) + "\n" + base64_encode(half.data(), half.size());
        return job;
    });
}

void ScreenScan::poll_find(Clock::time_point now) {
    PriceReply reply;
    if (!find_.poll(&reply)) return;
    find_busy_ = false;
    next_find_ = now + (reply.status == "ok" ? kFindGap : kFindRetry);
    if (reply.status != "ok") {
        poe_log("displays: SCANFIND: %s %s", reply.status.c_str(), reply.message.c_str());
        return;
    }
    if (!reply.plates.empty()) last_plate_ = now;
    // Where a tracked display was when the frame was taken.
    auto then = [&](const Tracked& t, float* x, float* y) {
        *x = t.x, *y = t.y;
        const Tracked::Sample* best = nullptr;
        for (const auto& s : t.history)
            if (!best || std::abs((s.at - frame_at_).count()) < std::abs((best->at - frame_at_).count())) best = &s;
        if (best) *x = best->x, *y = best->y;
    };
    for (const auto& p : reply.plates) {
        float cx = p[0] + p[2] / 2;
        // The nearest tracked display where this plate was. Displays stand
        // close together, so not far: a lost one that moved further is
        // added again (and read again) rather than risk taking another's
        // place, and its old self is forgotten.
        Tracked* match = nullptr;
        float nearest = 0;
        for (Tracked& t : tracked_) {
            float tx, ty;
            then(t, &tx, &ty);
            float reach = t.lost > 0 || t.seq == 0 ? 0.6f : 0.3f;
            float dx = std::abs(tx + t.w / 2 - cx), dy = std::abs(ty - p[1]);
            if (dx >= t.w * reach || dy >= t.h * 2 || std::abs(t.w - p[2]) >= t.w * 0.2f) continue;
            if (!match || dx + dy < nearest) match = &t, nearest = dx + dy;
        }
        if (!match) {
            add_plate(p, now);
        } else if (match->lost > 0) {
            // Lost while tracking (moved too fast, or covered): found again.
            match->x = p[0];
            match->y = p[1];
            match->lost = 0;
            match->moving = false;
            match->vx = match->vy = 0;
            match->samples.clear();
            match->seen = now;
            changed();
        }
    }
}

namespace {

// A template from grey pixels.
void make_template(const std::vector<uint8_t>& g, int w, int h, std::vector<float>* t, float* norm) {
    double mean = 0;
    for (uint8_t v : g) mean += v;
    mean /= std::max<size_t>(1, g.size());
    t->resize(g.size());
    double sq = 0;
    for (size_t i = 0; i < g.size(); i++) {
        (*t)[i] = (float)(g[i] - mean);
        sq += (double)(*t)[i] * (*t)[i];
    }
    *norm = (float)std::sqrt(sq);
    (void)w, (void)h;
}

}  // namespace

void ScreenScan::add_plate(const std::array<float, 8>& p, Clock::time_point now) {
    int px = (int)p[0], py = (int)p[1], pw = (int)p[2], ph = (int)p[3];
    int cx = (int)p[4], cy = (int)p[5], cw = (int)p[6], ch = (int)p[7];
    if (frame_.empty() || pw < 8 || ph < 8 || px < 0 || py < 0 || px + pw > frame_w_ || py + ph > frame_h_) return;
    if (cx < 0 || cy < 0 || cw <= 0 || ch <= 0 || cx + cw > frame_w_ || cy + ch > frame_h_) return;

    Tracked t;
    t.id = next_id_++;
    t.x = (float)px;
    t.y = (float)py;
    t.w = (float)pw;
    t.h = (float)ph;
    t.seen = t.sampled = now;
    auto grey = [&](int x0, int y0, int w, int h) {
        std::vector<uint8_t> g((size_t)w * h);
        for (int y = 0; y < h; y++)
            for (int x = 0; x < w; x++) g[(size_t)y * w + x] = luma(&frame_[((size_t)(y0 + y) * frame_w_ + x0 + x) * 3]);
        return g;
    };
    // `g` (w x h) shrunk by `k` into a template.
    auto set = [](Template& t, const std::vector<uint8_t>& g, int w, int h, int k) {
        std::vector<uint8_t> small = k == 1 ? g : shrink(g.data(), w, h, k, &t.w, &t.h);
        if (k == 1) t.w = w, t.h = h;
        make_template(small, t.w, t.h, &t.v, &t.norm);
    };
    // The plate, to track it by.
    std::vector<uint8_t> grey_plate = grey(px, py, pw, ph);
    auto plate = std::make_shared<Plate>();
    set(plate->t4, grey_plate, pw, ph, 4);
    set(plate->t2, grey_plate, pw, ph, 2);
    set(plate->t1, grey_plate, pw, ph, 1);
    t.plate = plate;
    // The name under it, to know it again.
    int nh = (int)(ph * kNameBand);
    if (py + ph + nh <= frame_h_) set(t.name, grey(px, py + ph, pw, nh), pw, nh, 4);

    auto same_name = [&](const Template& a) {
        if (t.name.v.empty() || a.w != t.name.w || a.h != t.name.h || a.norm < 1e-3f || t.name.norm < 1e-3f)
            return false;
        double dot = 0;
        for (size_t i = 0; i < a.v.size(); i++) dot += (double)a.v[i] * t.name.v[i];
        return dot / ((double)a.norm * t.name.norm) >= kSameName;
    };
    // Seen in the last few minutes, by the look of it: its price at once,
    // until reading it says otherwise.
    for (auto it = remembered_.rbegin(); it != remembered_.rend(); ++it) {
        if (!same_name(it->name)) continue;
        t.reply = it->reply;
        t.replied = true;
        t.verify = true;
        remembered_.erase(std::next(it).base());
        poe_log("displays: plate at %d,%d looks like %s again", px, py, t.reply.title.c_str());
        break;
    }

    // Its tooltip's pixels, to read and price (encoded off the game's thread).
    std::vector<uint8_t> crop((size_t)cw * ch * 3);
    for (int y = 0; y < ch; y++)
        memcpy(&crop[(size_t)y * cw * 3], &frame_[((size_t)(cy + y) * frame_w_ + cx) * 3], (size_t)cw * 3);
    char header[96];
    snprintf(header, sizeof header, "%d %d %d %d %d %d", cw, ch, px - cx, py - cy, pw, ph);
    t.request_job = std::async(std::launch::async, [crop = std::move(crop), header = std::string(header)]() {
        return "SCANITEM\n" + header + "\n" + base64_encode(crop.data(), crop.size());
    });
    t.client = std::make_unique<DaemonClient>();
    t.retry_at = now;
    if (!t.verify) poe_log("displays: plate at %d,%d (%dx%d)", px, py, pw, ph);
    tracked_.push_back(std::move(t));
    changed();
}

std::vector<ScreenScan::Region> ScreenScan::regions_wanted() const {
    std::vector<Region> out;
    if (!active_) return out;
    for (const Tracked& t : tracked_) {
        // Room for the camera to move between looks (a frame or two), around
        // where it should be by now.
        float dt = t.moving ? std::min(kMaxPredict, std::chrono::duration<float>(now_ - t.sampled).count()) : 0;
        float ex = t.x + t.vx * dt, ey = t.y + t.vy * dt;
        // Lost for a while: SCANFIND looks for it, not us.
        if (t.lost >= kStopLooking) continue;
        // Wider before its first sample (the plate was found in a frame from
        // a moment ago, and may have moved since) and right after losing it.
        bool unsure = t.seq == 0 || t.lost > 0;
        // Moving: room for 60 ms at its speed, on top of the prediction's.
        float speed = std::max(std::abs(t.vx), std::abs(t.vy));
        int m = unsure ? std::max(96, (int)t.h * 2) : std::max({24, (int)t.h / 2, (int)(speed * 0.06f)});
        Region r;
        r.id = t.id;
        r.x = std::max(0, (int)ex - m);
        r.y = std::max(0, (int)ey - m);
        r.w = std::min(extent_w_, (int)(ex + t.w) + m) - r.x;
        r.h = std::min(extent_h_, (int)(ey + t.h) + m) - r.y;
        if (r.w >= t.w && r.h >= t.h) out.push_back(r);
    }
    return out;
}

void ScreenScan::regions_taken(std::vector<Region> regions, std::vector<std::vector<uint8_t>> raw, ToRgb to_rgb,
                               Clock::time_point at, uint64_t seq) {
    Worker::Job job;
    for (const Region& r : regions) {
        std::shared_ptr<const Plate> plate;
        for (const Tracked& t : tracked_)
            if (t.id == r.id) plate = t.plate;
        job.plates.push_back(plate);
    }
    job.regions = std::move(regions);
    job.raw = std::move(raw);
    job.to_rgb = std::move(to_rgb);
    job.at = at;
    job.seq = seq;
    {
        std::lock_guard lk(worker_->lock);
        worker_->job = std::move(job);
    }
    worker_->wake.notify_one();
}

void ScreenScan::apply(const Match& m) {
    Tracked* t = nullptr;
    for (Tracked& c : tracked_)
        if (c.id == m.id) t = &c;
    if (!t || m.seq <= t->seq) return;
    t->seq = m.seq;
    if (!m.found) {
        if (track_log())
            poe_log("track %llu seq %llu lost (score %.2f)", (unsigned long long)t->id, (unsigned long long)m.seq,
                    m.score);
        if (t->lost++ == kHideAfter) changed();
        t->moving = false;
        t->vx = t->vy = 0;
        t->samples.clear();
        return;
    }
    Clock::time_point at = m.at;
    float full = m.score;
    float nx = m.x, ny = m.y;
    t->history.push_back({at, nx, ny});
    while (t->history.size() > 2 && at - t->history.front().at > std::chrono::seconds(2))
        t->history.erase(t->history.begin());
    // The speed from the oldest sample within the span to this one.
    t->samples.push_back({at, nx, ny});
    while (t->samples.size() > 2 && at - t->samples[1].at >= kVelocitySpan) t->samples.erase(t->samples.begin());
    const auto& first = t->samples.front();
    float span = std::chrono::duration<float>(at - first.at).count();
    t->moving = at - first.at >= kVelocityMin && at - first.at <= kVelocitySpan * 3;
    t->vx = t->moving ? (nx - first.x) / span : 0;
    t->vy = t->moving ? (ny - first.y) / span : 0;
    if (t->lost >= kHideAfter || std::abs(nx - t->x) >= 1 || std::abs(ny - t->y) >= 1) changed();
    if (track_log())
        poe_log("track %llu seq %llu t %.4f x %.0f y %.0f vx %.0f score %.2f", (unsigned long long)t->id,
                (unsigned long long)m.seq, std::chrono::duration<double>(at.time_since_epoch()).count(), nx, ny,
                t->vx, full);
    t->x = nx;
    t->y = ny;
    t->sampled = at;
    t->lost = 0;
    t->seen = now_;
}

std::vector<ScreenScan::Label> ScreenScan::labels() const {
    std::vector<Label> out;
    if (!active_) return out;
    double best = 0;
    for (const Tracked& t : tracked_)
        if (t.lost < kHideAfter && t.replied && t.reply.status == "ok") best = std::max(best, t.reply.value_chaos);
    for (const Tracked& t : tracked_) {
        if (t.lost >= kHideAfter) continue;
        // Not an item (another kind of tooltip with a plate): no label.
        if (t.replied && t.reply.status == "no_item") continue;
        Label l;
        // Where it is by now: its last place, moved on at its speed.
        float dt = t.moving ? std::clamp(std::chrono::duration<float>(now_ - t.sampled).count(), 0.0f, kMaxPredict) : 0;
        l.plate_x = t.x + t.vx * dt;
        l.plate_y = t.y + t.vy * dt;
        l.seen_x = t.x;
        l.seen_y = t.y;
        l.plate = t.plate;
        l.x = l.plate_x + t.w / 2;
        l.y = l.plate_y;
        if (!t.replied) {
            l.amount = "…";
            l.pending = true;
        } else if (t.reply.status == "ok") {
            bool divine = t.reply.value_divine >= 1;
            l.amount = amount_text(divine ? t.reply.value_divine : t.reply.value_chaos);
            l.icon = divine ? "divine" : "chaos";
            l.price = t.reply.value_text;
            l.best = best > 0 && t.reply.value_chaos >= best;
        } else {
            l.amount = t.reply.status == "not_found" ? "?" : "!";
            l.note = t.reply.message;
        }
        if (l.price.empty()) l.price = l.amount;
        l.title = t.reply.title;
        if (!t.reply.detail.empty()) l.title += l.title.empty() ? t.reply.detail : " · " + t.reply.detail;
        out.push_back(std::move(l));
    }
    return out;
}
