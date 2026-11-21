#include "screen_scan.h"
#include "game.h"
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

// Between SCANFINDs: often enough to catch displays coming into view...
constexpr auto kFindGap = std::chrono::milliseconds(300);
// ...and while idle in Heist, waiting for one; back to idle after this
// long without a display's tag in view.
constexpr auto kIdleFindGap = std::chrono::seconds(1);
constexpr auto kIdleAfter = std::chrono::seconds(5);
// POE_WAYTRADE_SCAN_SAVE: at most one frame saved this often.
constexpr auto kSaveGap = std::chrono::seconds(5);
// After the daemon couldn't take a frame (or none could be copied).
constexpr auto kFindRetry = std::chrono::seconds(1);
constexpr auto kAreaGap = std::chrono::seconds(1);
// Turned on by hand where the area isn't known: off after this long
// without a display in sight.
constexpr auto kIdleOff = std::chrono::minutes(3);
// A tooltip not found for this long is forgotten (SCANFIND finds it again
// if it comes back; its price is remembered, see kRemember).
constexpr auto kForget = std::chrono::seconds(4);
// Not found (covered by another tooltip, or moved too fast): its label
// hides after this long, and it isn't looked for each frame after the
// next (SCANFIND finds it again, a few times a second). Meanwhile it's
// taken to move with the camera, like the displays still found.
constexpr auto kHideAfter = std::chrono::milliseconds(150);
constexpr auto kStopLooking = std::chrono::seconds(1);
// A display read as partly hidden this many times is taken as no item.
constexpr int kMaxRereads = 4;
// A display read and priced is read again this many times, this far apart,
// in case another tooltip covered part of it the first time: the read with
// the most lines wins. A rare's reads add up (the daemon gathers its mods):
// it's read again while they still add something, up to kMaxFollowUps.
constexpr int kFollowUps = 2;
constexpr int kMaxFollowUps = 8;
constexpr auto kFollowUpGap = std::chrono::milliseconds(1500);
// The daemon is still loading prices or names: ask again after this.
constexpr auto kRetryLoading = std::chrono::seconds(2);
// A display read as partly hidden: read again (from a newer frame) after this.
constexpr auto kReread = std::chrono::seconds(1);
// Tracking: how well the plate and name must match where they're looked for.
constexpr float kTrackScore = 0.7f;
// A tooltip partly off screen is tracked by what's on it: at least this
// much of the template, and of its width and height.
constexpr float kMinVisible = 0.35f;
constexpr float kMinVisibleSide = 0.4f;
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
// Normalized cross-correlation of a template (any mean) with the part of it
// inside the image, at (x, y), which may be partly outside: -2 if less than
// kMinVisible of it is in.
float ncc_clipped(const uint8_t* img, int iw, int ih, const std::vector<float>& t, int tw, int th, int x, int y) {
    int x0 = std::max(0, -x), y0 = std::max(0, -y), x1 = std::min(tw, iw - x), y1 = std::min(th, ih - y);
    if (x1 <= x0 || y1 <= y0) return -2;
    int n = (x1 - x0) * (y1 - y0);
    if (n < kMinVisible * tw * th) return -2;
    double sv = 0, svv = 0, st = 0, stt = 0, svt = 0;
    for (int ty = y0; ty < y1; ty++) {
        const uint8_t* row = img + (size_t)(y + ty) * iw + x;
        const float* trow = t.data() + (size_t)ty * tw;
        for (int tx = x0; tx < x1; tx++) {
            double v = row[tx], tv = trow[tx];
            sv += v;
            svv += v * v;
            st += tv;
            stt += tv * tv;
            svt += v * tv;
        }
    }
    double vv = svv - sv * sv / n, tt = stt - st * st / n;
    if (vv < 1 || tt < 1e-6) return 0;
    return (float)((svt - sv * st / n) / std::sqrt(vv * tt));
}

// The best place for template `t` in image `g` (w x h, a level of region `r`
// shrunk by `k`) within [x_lo, x_hi] x [y_lo, y_hi], which may run past the
// image where the screen cuts the region. In-image places take the fast path.
float best_in(const uint8_t* g, int w, int h, const ScreenScan::Template& t, int x_lo, int x_hi, int y_lo, int y_hi,
              int* bx, int* by) {
    float best = -2;
    for (int y = y_lo; y <= y_hi; y++)
        for (int x = x_lo; x <= x_hi; x++) {
            bool inside = x >= 0 && y >= 0 && x + t.w <= w && y + t.h <= h;
            float s = inside ? ncc(g, w, t.v, t.norm, t.w, t.h, x, y) : ncc_clipped(g, w, h, t.v, t.w, t.h, x, y);
            if (s > best) best = s, *bx = x, *by = y;
        }
    return best;
}

// How far a template (tw x th) may start, in an image w x h, on a region cut
// by the screen on some sides: past a cut side, as long as enough stays in.
void place_range(const ScreenScan::Region& r, int w, int h, int tw, int th, int* x_lo, int* x_hi, int* y_lo,
                 int* y_hi) {
    int hang_x = (int)(tw * (1 - kMinVisibleSide)), hang_y = (int)(th * (1 - kMinVisibleSide));
    *x_lo = r.cut_left ? -hang_x : 0;
    *x_hi = r.cut_right ? w - tw + hang_x : w - tw;
    *y_lo = r.cut_top ? -hang_y : 0;
    *y_hi = r.cut_bottom ? h - th + hang_y : h - th;
}

ScreenScan::Match match_plate(const ScreenScan::Plate& p, const ScreenScan::Region& r, const uint8_t* g1) {
    ScreenScan::Match m;
    int w4, h4, w2, h2, lo_x, hi_x, lo_y, hi_y;
    // Coarse at quarter size, everywhere in the region...
    std::vector<uint8_t> g4 = shrink(g1, r.w, r.h, 4, &w4, &h4);
    int bx = 0, by = 0;
    place_range(r, w4, h4, p.t4.w, p.t4.h, &lo_x, &hi_x, &lo_y, &hi_y);
    if (best_in(g4.data(), w4, h4, p.t4, lo_x, hi_x, lo_y, hi_y, &bx, &by) <= -2) return m;
    // ...then at half size around it...
    std::vector<uint8_t> g2 = shrink(g1, r.w, r.h, 2, &w2, &h2);
    int hx = bx * 2, hy = by * 2;
    place_range(r, w2, h2, p.t2.w, p.t2.h, &lo_x, &hi_x, &lo_y, &hi_y);
    float half = best_in(g2.data(), w2, h2, p.t2, std::max(lo_x, bx * 2 - 2), std::min(hi_x, bx * 2 + 2),
                         std::max(lo_y, by * 2 - 2), std::min(hi_y, by * 2 + 2), &hx, &hy);
    m.score = half;
    if (half < kTrackScore) return m;
    // ...and to the pixel at full size.
    int fx = hx * 2, fy = hy * 2;
    place_range(r, r.w, r.h, p.t1.w, p.t1.h, &lo_x, &hi_x, &lo_y, &hi_y);
    float full = best_in(g1, r.w, r.h, p.t1, std::max(lo_x, hx * 2 - 1), std::min(hi_x, hx * 2 + 1),
                         std::max(lo_y, hy * 2 - 1), std::min(hi_y, hy * 2 + 1), &fx, &fy);
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

ScreenScan::ScreenScan() : worker_(std::make_unique<Worker>()), log_hint_(guess_log()) {
    if (const char* dir = getenv("POE_WAYTRADE_SCAN_SAVE")) save_dir_ = dir;
}

ScreenScan::~ScreenScan() = default;

void ScreenScan::start(bool automatic, bool idling, Clock::time_point now) {
    tracked_.clear();
    remembered_.clear();
    not_items_.clear();
    active_ = true;
    automatic_ = automatic;
    idling_ = idle_ = idling;
    started_ = last_plate_ = last_tag_ = now;
    next_find_ = now;
    poe_log("displays: on (%s)", idling ? "Heist: idle until a display shows" : automatic ? "auto-watch area" : "hotkey");
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
    not_items_.clear();
    idle_ = idling_ = false;
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
        // POE_WAYTRADE_SCAN_SAVE: the hotkey saves the next frame instead
        // (turning scanning on if need be).
        if (!save_dir_.empty()) {
            save_next_ = true;
            if (!active_) start(false, false, now);
            continue;
        }
        if (active_) {
            stop("hotkey");
            suppressed_ = true;  // don't come straight back on in this area
        } else {
            suppressed_ = false;
            start(false, false, now);
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
    measure_camera();

    for (Tracked& t : tracked_) {
        if (t.request_job.valid() && t.request_job.wait_for(std::chrono::seconds(0)) == std::future_status::ready)
            t.request = t.request_job.get();
        if (t.client && !t.request.empty() && !t.client->busy() && (!t.replied || t.verify || t.following_up) &&
            now >= t.retry_at)
            t.client->send_raw(t.request);
        PriceReply reply;
        if (t.client && t.client->poll(&reply)) {
            if (reply.status == "loading") {
                t.retry_at = now + kRetryLoading;
                continue;
            }
            if (t.following_up) {
                // A second look: taken if it saw more of the tooltip.
                t.following_up = false;
                t.request.clear();
                size_t n = reply.lines.size();
                bool priced = reply.status == "ok" || reply.status == "not_found";
                bool more = reply.facts > t.facts || (reply.facts == t.facts && n > t.read_lines);
                if (!priced || !more) {
                    // Nothing new this time: the next look, if any left.
                    if (t.follow_ups < kFollowUps && t.total_follow_ups < kMaxFollowUps) {
                        t.follow_ups++;
                        t.total_follow_ups++;
                        t.reread_at = now + kFollowUpGap;
                    }
                    continue;
                }
                poe_log("displays: read more of %s (%zu lines, %d facts; was %zu, %d)", reply.title.c_str(), n,
                        reply.facts, t.read_lines, t.facts);
                // Still learning: more looks.
                if (reply.facts > t.facts && t.total_follow_ups < kMaxFollowUps) t.follow_ups = 0;
            }
            bool gave_up = false;
            // Not an item, says the first read: one more look from a later
            // frame first (another tooltip may have covered all of it).
            if (reply.status == "no_item" && t.rereads == 0) reply.status = "unreadable";
            if (reply.status == "unreadable") {
                // Covered by another tooltip: crop it again from a later frame.
                if (++t.rereads < kMaxRereads) {
                    t.reread_at = now + kReread;
                    t.request.clear();
                    continue;
                }
                reply.status = "no_item";
                gave_up = true;
            }
            std::string lines;
            for (const auto& l : reply.lines) lines += (lines.empty() ? "" : " | ") + l;
            poe_log("displays: %s %s (%s) %s%s%s [%s]", reply.status.c_str(), reply.title.c_str(),
                    reply.detail.c_str(), reply.value_text.c_str(), reply.message.empty() ? "" : ": ",
                    reply.message.c_str(), lines.c_str());
            t.read_lines = reply.lines.size();
            t.facts = reply.facts;
            reply.lines.clear();
            if ((reply.status == "ok" || reply.status == "not_found") && t.follow_ups < kFollowUps &&
                t.total_follow_ups < kMaxFollowUps) {
                t.follow_ups++;
                t.total_follow_ups++;
                t.reread_at = now + kFollowUpGap;
            }
            // (Not one that only stayed hidden: it may be read when seen again.)
            if (reply.status == "no_item" && !gave_up && !t.name.v.empty()) not_items_.push_back(t.name);
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
    dedupe();
    if (tracked_.size() != before) changed();
    for (Tracked& t : tracked_)
        if (hidden(t) != t.was_hidden) t.was_hidden = !t.was_hidden, changed();

    // Heist: back to idle once no display's tag has been in view for a while.
    if (idling_ && !idle_) {
        for (const Tracked& t : tracked_)
            if (!hidden(t) && !t.duplicate && !(t.replied && t.reply.status == "no_item")) last_tag_ = now;
        if (now - last_tag_ > kIdleAfter) {
            for (Tracked& t : tracked_) forget(t);
            tracked_.clear();
            idle_ = true;
            next_find_ = now + kIdleFindGap;
            poe_log("displays: no display in view: idle");
            changed();
        }
    }
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
            bool watch = reply.watch == "1" || reply.watch == "idle";
            if (watch && !suppressed_ && !active_)
                start(true, reply.watch == "idle", now);
            else if (!watch && active_ && automatic_)
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
    // POE_WAYTRADE_SCAN_SAVE, after a hotkey press: this frame, as a PPM.
    std::string save;
    if (save_next_ && now_ - saved_at_ > kSaveGap) {
        save_next_ = false;
        saved_at_ = now_;
        save = save_dir_ + "/frame-" + std::to_string(std::chrono::system_clock::now().time_since_epoch().count()) +
               ".ppm";
    }
    frame_job_ = std::async(std::launch::async, [px, to_rgb, width, height, save]() {
        FrameJob job;
        job.rgb.resize((size_t)width * height * 3);
        if (!to_rgb(px, (size_t)width * height, job.rgb.data())) return FrameJob{};
        if (!save.empty()) {
            if (FILE* f = fopen(save.c_str(), "wb")) {
                fprintf(f, "P6\n%d %d\n255\n", width, height);
                fwrite(job.rgb.data(), 1, job.rgb.size(), f);
                fclose(f);
                poe_log("displays: frame saved to %s", save.c_str());
            } else {
                poe_log("displays: can't save a frame to %s", save.c_str());
            }
        }
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
    next_find_ = now + (reply.status != "ok" ? kFindRetry : idle_ ? kIdleFindGap : kFindGap);
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
    // How far the camera has moved the displays since that frame.
    float shift_x = 0, shift_y = 0;
    bool shifted = camera_shift(&shift_x, &shift_y);
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
            // A display (not a known non-item) wakes an idle scan.
            if (add_plate(p, now, shifted, shift_x, shift_y) && idle_) {
                idle_ = false;
                last_tag_ = now;
                next_find_ = now + kFindGap;
                poe_log("displays: a display in view: full speed");
                changed();
            }
        } else if (match->lost > 0) {
            // Lost while tracking (moved too fast, or covered): found again.
            match->x = p[0] + shift_x;
            match->y = p[1] + shift_y;
            match->lost = 0;
            match->moving = shifted && camera_;
            match->vx = match->moving ? camera_vx_ : 0;
            match->vy = match->moving ? camera_vy_ : 0;
            if (match->moving) match->sampled = camera_at_;
            match->samples.clear();
            match->seen = now;
            changed();
        }
    }
    // Partly hidden displays, cropped again from this frame where they are.
    for (Tracked& t : tracked_) {
        if (!t.reread_at || now < *t.reread_at || t.lost != 0 || t.verify || !t.request.empty() ||
            t.request_job.valid())
            continue;
        float tx, ty;
        then(t, &tx, &ty);
        int px = (int)tx, py = (int)ty, cx = px + t.crop_dx, cy = py + t.crop_dy;
        int x0 = std::max(0, cx), y0 = std::max(0, cy);
        int x1 = std::min(frame_w_, cx + t.crop_w), y1 = std::min(frame_h_, cy + t.crop_h);
        if (x1 - x0 < t.crop_w / 2 || y1 - y0 < t.crop_h / 4 || px < x0 || py < y0) continue;
        t.request_job = crop_request(x0, y0, x1 - x0, y1 - y0, px, py, (int)t.w, (int)t.h);
        t.reread_at.reset();
        t.following_up = t.replied;
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

bool ScreenScan::camera_shift(float* dx, float* dy) const {
    // From each display found now and where it was in that frame (its
    // history), the median: one the game's layout pushed aside doesn't count.
    std::vector<float> xs, ys;
    for (const Tracked& t : tracked_) {
        if (t.lost != 0 || t.seq == 0) continue;
        const Tracked::Sample* best = nullptr;
        for (const auto& s : t.history)
            if (!best || std::abs((s.at - frame_at_).count()) < std::abs((best->at - frame_at_).count())) best = &s;
        if (!best || std::abs(std::chrono::duration<float>(best->at - frame_at_).count()) > 0.05f) continue;
        xs.push_back(t.x - best->x);
        ys.push_back(t.y - best->y);
    }
    if (xs.empty()) {
        // None to go by: the camera's last known speed, if any.
        if (!camera_) return false;
        float dt = std::chrono::duration<float>(camera_at_ - frame_at_).count();
        *dx = camera_vx_ * dt, *dy = camera_vy_ * dt;
        return true;
    }
    std::nth_element(xs.begin(), xs.begin() + xs.size() / 2, xs.end());
    std::nth_element(ys.begin(), ys.begin() + ys.size() / 2, ys.end());
    *dx = xs[xs.size() / 2], *dy = ys[ys.size() / 2];
    return true;
}

void ScreenScan::measure_camera() {
    // The displays stand in the world: found ones move together, by the
    // camera. Their median speed, for those lost a moment.
    std::vector<float> xs, ys;
    Clock::time_point at{};
    for (const Tracked& t : tracked_) {
        if (t.lost != 0 || !t.moving) continue;
        xs.push_back(t.vx);
        ys.push_back(t.vy);
        at = std::max(at, t.sampled);
    }
    camera_ = !xs.empty();
    if (!camera_) return;
    std::nth_element(xs.begin(), xs.begin() + xs.size() / 2, xs.end());
    std::nth_element(ys.begin(), ys.begin() + ys.size() / 2, ys.end());
    camera_vx_ = xs[xs.size() / 2], camera_vy_ = ys[ys.size() / 2];
    camera_at_ = at;
}

bool ScreenScan::hidden(const Tracked& t) const { return t.lost > 0 && now_ - t.seen >= kHideAfter; }

bool ScreenScan::add_plate(const std::array<float, 8>& p, Clock::time_point now, bool shifted, float shift_x,
                           float shift_y) {
    int px = (int)p[0], py = (int)p[1], pw = (int)p[2], ph = (int)p[3];
    int cx = (int)p[4], cy = (int)p[5], cw = (int)p[6], ch = (int)p[7];
    if (frame_.empty() || pw < 8 || ph < 8 || px < 0 || py < 0 || px + pw > frame_w_ || py + ph > frame_h_) return false;
    if (cx < 0 || cy < 0 || cw <= 0 || ch <= 0 || cx + cw > frame_w_ || cy + ch > frame_h_) return false;

    Tracked t;
    t.id = next_id_++;
    // Where it is by now: moved since that frame along with the displays
    // being tracked (the frame is a fifth of a second old or so).
    t.x = (float)px + shift_x;
    t.y = (float)py + shift_y;
    t.w = (float)pw;
    t.h = (float)ph;
    t.seen = t.sampled = now;
    if (shifted && camera_) t.moving = true, t.vx = camera_vx_, t.vy = camera_vy_, t.sampled = camera_at_;
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
    // The plate and the name under it, to track it by (just the plate if the
    // name is off the bottom of the screen).
    int nh = (int)(ph * kNameBand);
    bool name_fits = py + ph + nh <= frame_h_;
    int th = name_fits ? ph + nh : ph;
    std::vector<uint8_t> grey_plate = grey(px, py, pw, th);
    auto plate = std::make_shared<Plate>();
    set(plate->t4, grey_plate, pw, th, 4);
    set(plate->t2, grey_plate, pw, th, 2);
    set(plate->t1, grey_plate, pw, th, 1);
    t.plate = plate;
    t.th = (float)th;
    // The name alone, to know it again.
    if (name_fits) set(t.name, grey(px, py + ph, pw, nh), pw, nh, 4);

    auto same_name = [&](const Template& a) {
        if (t.name.v.empty() || a.w != t.name.w || a.h != t.name.h || a.norm < 1e-3f || t.name.norm < 1e-3f)
            return false;
        double dot = 0;
        for (size_t i = 0; i < a.v.size(); i++) dot += (double)a.v[i] * t.name.v[i];
        return dot / ((double)a.norm * t.name.norm) >= kSameName;
    };
    // Read before as no item (another kind of tooltip with a plate): skip it.
    for (const Template& n : not_items_)
        if (same_name(n)) return false;
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

    // Its tooltip's pixels, to read and price.
    t.crop_dx = cx - px;
    t.crop_dy = cy - py;
    t.crop_w = cw;
    t.crop_h = ch;
    t.request_job = crop_request(cx, cy, cw, ch, px, py, pw, ph);
    t.client = std::make_unique<DaemonClient>();
    t.retry_at = now;
    if (!t.verify) poe_log("displays: plate at %d,%d (%dx%d)", px, py, pw, ph);
    tracked_.push_back(std::move(t));
    changed();
    return true;
}

namespace {

// What a reply says the item is ("" while unknown): no two displays show the
// same one.
std::string identity(const PriceReply& r) {
    if (r.status != "ok" && r.status != "not_found") return "";
    return r.title + "\n" + r.detail;
}

}  // namespace

void ScreenScan::dedupe() {
    auto known = [](const Tracked& t) { return t.replied && !t.verify ? identity(t.reply) : std::string(); };
    // Which of two to keep: one tracking now over a lost one, then one with
    // its price, then the older.
    auto better = [](const Tracked& a, const Tracked& b) {
        if ((a.lost == 0) != (b.lost == 0)) return a.lost == 0;
        if (a.replied != b.replied) return a.replied;
        return a.id < b.id;
    };
    // Two trackers on one plate (both where they were just found): one goes.
    std::vector<bool> drop(tracked_.size());
    for (size_t i = 0; i < tracked_.size(); i++) {
        for (size_t j = i + 1; j < tracked_.size() && !drop[i]; j++) {
            Tracked& a = tracked_[i];
            Tracked& b = tracked_[j];
            if (drop[j] || a.lost != 0 || b.lost != 0 || std::abs(a.x - b.x) >= a.w / 2 || std::abs(a.y - b.y) >= a.h)
                continue;
            bool keep_a = better(a, b);
            Tracked& keep = keep_a ? a : b;
            Tracked& lose = keep_a ? b : a;
            if (!keep.replied && lose.replied) {
                keep.reply = lose.reply;
                keep.replied = true;
                keep.verify = lose.verify;
            }
            drop[keep_a ? j : i] = true;
        }
    }
    // A display lost (gone off screen, say) and found again is added anew
    // and read: once it reads as the same item, the lost one is the same
    // display, and goes. (Kept, it would take the tag back whenever the new
    // one missed a frame, and its tag is hidden: the tag would blink.)
    for (size_t i = 0; i < tracked_.size(); i++) {
        std::string k = known(tracked_[i]);
        if (k.empty() || tracked_[i].lost == 0) continue;
        for (size_t j = 0; j < tracked_.size(); j++)
            if (j != i && !drop[j] && tracked_[j].lost == 0 && known(tracked_[j]) == k) drop[i] = true;
    }
    size_t i = 0;
    std::erase_if(tracked_, [&](const Tracked&) { return drop[i++]; });
    // One item on two displays found at once: only the older one's tag
    // shows (the same one each frame, so the tag doesn't flip between them).
    for (Tracked& t : tracked_) {
        bool was = t.duplicate;
        t.duplicate = false;
        std::string k = known(t);
        if (!k.empty())
            for (const Tracked& o : tracked_)
                if (&o != &t && known(o) == k && o.id < t.id) t.duplicate = true;
        if (t.duplicate && !was) poe_log("displays: %s twice: showing one", t.reply.title.c_str());
        if (t.duplicate != was) changed();
    }
    // An item being tracked isn't also one remembered out of sight.
    std::erase_if(remembered_, [&](const Remembered& r) {
        std::string k = identity(r.reply);
        return !k.empty() && std::any_of(tracked_.begin(), tracked_.end(), [&](const Tracked& t) { return known(t) == k; });
    });
}

std::future<std::string> ScreenScan::crop_request(int cx, int cy, int cw, int ch, int px, int py, int pw,
                                                   int ph) const {
    std::vector<uint8_t> crop((size_t)cw * ch * 3);
    for (int y = 0; y < ch; y++)
        memcpy(&crop[(size_t)y * cw * 3], &frame_[((size_t)(cy + y) * frame_w_ + cx) * 3], (size_t)cw * 3);
    char header[96];
    snprintf(header, sizeof header, "%d %d %d %d %d %d", cw, ch, px - cx, py - cy, pw, ph);
    return std::async(std::launch::async, [crop = std::move(crop), header = std::string(header)]() {
        return "SCANITEM\n" + header + "\n" + base64_encode(crop.data(), crop.size());
    });
}

std::vector<ScreenScan::Region> ScreenScan::regions_wanted() const {
    std::vector<Region> out;
    if (!tracking()) return out;
    for (const Tracked& t : tracked_) {
        // Room for the camera to move between looks (a frame or two), around
        // where it should be by now.
        float dt = t.moving ? std::min(kMaxPredict, std::chrono::duration<float>(now_ - t.sampled).count()) : 0;
        float ex = t.x + t.vx * dt, ey = t.y + t.vy * dt;
        // Lost for a while: SCANFIND looks for it, not us.
        if (t.lost > 0 && now_ - t.seen >= kStopLooking) continue;
        // Wider before its first sample (the plate was found in a frame from
        // a moment ago, and may have moved since) and right after losing it.
        bool unsure = t.seq == 0 || t.lost > 0;
        // Moving: room for 60 ms at its speed, on top of the prediction's.
        float speed = std::max(std::abs(t.vx), std::abs(t.vy));
        int m = unsure ? std::max(96, (int)t.h * 2) : std::max({24, (int)t.h / 2, (int)(speed * 0.06f)});
        Region r;
        r.id = t.id;
        int x0 = (int)ex - m, y0 = (int)ey - m, x1 = (int)(ex + t.w) + m, y1 = (int)(ey + t.th) + m;
        r.x = std::max(0, x0);
        r.y = std::max(0, y0);
        r.w = std::min(extent_w_, x1) - r.x;
        r.h = std::min(extent_h_, y1) - r.y;
        // Cut by the screen: matched on the part that's on it.
        r.cut_left = x0 < 0;
        r.cut_top = y0 < 0;
        r.cut_right = x1 > extent_w_;
        r.cut_bottom = y1 > extent_h_;
        if (r.w >= t.w * kMinVisibleSide && r.h >= t.th * kMinVisibleSide) out.push_back(r);
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
        t->lost++;
        t->samples.clear();
        // Moved on with the camera meanwhile, to be looked for where it is.
        if (camera_) {
            float dt = std::chrono::duration<float>(m.at - t->sampled).count();
            if (dt > 0) {
                t->x += camera_vx_ * dt;
                t->y += camera_vy_ * dt;
                t->sampled = m.at;
                t->history.push_back({m.at, t->x, t->y});
            }
            t->moving = true;
            t->vx = camera_vx_;
            t->vy = camera_vy_;
        } else {
            t->moving = false;
            t->vx = t->vy = 0;
        }
        changed();
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
    if (hidden(*t) || std::abs(nx - t->x) >= 1 || std::abs(ny - t->y) >= 1) changed();
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
    if (!tracking()) return out;
    // The most valuable of everything seen in this area, in sight or not.
    double best = 0;
    for (const Tracked& t : tracked_)
        if (t.replied && t.reply.status == "ok") best = std::max(best, t.reply.value_chaos);
    for (const Remembered& r : remembered_)
        if (r.reply.status == "ok") best = std::max(best, r.reply.value_chaos);
    for (const Tracked& t : tracked_) {
        if (hidden(t)) continue;
        // Not an item (another kind of tooltip with a plate), or an item
        // another display's tag shows already: no label.
        if ((t.replied && t.reply.status == "no_item") || t.duplicate) continue;
        Label l;
        l.id = t.id;
        // Where it is by now: its last place, moved on at its speed.
        float dt = t.moving ? std::clamp(std::chrono::duration<float>(now_ - t.sampled).count(), 0.0f, kMaxPredict) : 0;
        l.plate_x = t.x + t.vx * dt;
        l.plate_y = t.y + t.vy * dt;
        l.seen_x = t.x;
        l.seen_y = t.y;
        l.vx = t.moving ? t.vx : 0;
        l.vy = t.moving ? t.vy : 0;
        l.plate = t.plate;
        l.x = l.plate_x + t.w / 2;
        l.y = l.plate_y;
        if (!t.replied) {
            l.amount = "…";
            l.pending = true;
        } else if (t.reply.status == "ok") {
            bool divine = t.reply.value_divine >= 1;
            l.amount = amount_text(divine ? t.reply.value_divine : t.reply.value_chaos);
            l.icon = divine ? "divine" : small_unit_id();
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
