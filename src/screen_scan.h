// Prices item displays (vault reliquaries and the like), whose world-space
// tooltips can't be copied with Ctrl+C: while on, a few times a second the
// whole frame goes to the daemon, which finds the tooltips' title plates
// (SCANFIND); each new one's pixels go back to be read and priced
// (SCANITEM), and a label with its price follows it on screen, tracked
// frame by frame from small copies around it.
//
// On with its hotkey, or by itself on entering one of the settings'
// auto_watch_areas (the daemon follows the game's Client.txt: AREA). Off
// with the hotkey, on leaving the area, or (without a log to follow) after a
// while without a display in sight.
#pragma once

#include "daemon_client.h"

#include <array>
#include <chrono>
#include <cstdint>
#include <functional>
#include <future>
#include <memory>
#include <string>
#include <vector>

class ScreenScan {
public:
    using Clock = std::chrono::steady_clock;

    ScreenScan();
    ~ScreenScan();

    // Once per frame. `toggles`: presses of the hotkey since last time.
    void update(Clock::time_point now, int toggles);
    bool active() const { return active_; }

    // overlay.cpp: a copy of the whole frame wanted (for SCANFIND)...
    bool frame_wanted(Clock::time_point now) const;
    void frame_started() { frame_pending_ = true; }
    // ...and here it is, as the swapchain's pixels (4 bytes each, rows top
    // to bottom) with what turns them into RGB, or not (`px` null). Read on
    // a worker thread (megabytes: too slow for the game's frame), so `px`
    // must stay valid while frame_busy(). `at`: when it was presented.
    using ToRgb = std::function<bool(const uint8_t* px, size_t count, uint8_t* rgb)>;
    void frame_taken(int width, int height, const uint8_t* px, ToRgb to_rgb, Clock::time_point at);
    bool frame_busy() const { return frame_job_.valid(); }
    // Before the frame's memory goes away.
    void wait_frame() {
        if (frame_job_.valid()) frame_job_.wait();
    }

    // Small copies around each tracked tooltip's plate, every frame (several
    // can be on their way at once).
    struct Region {
        uint64_t id = 0;  // the tooltip's
        int x = 0, y = 0, w = 0, h = 0;
    };
    std::vector<Region> regions_wanted() const;
    // The regions asked for (same order), as the swapchain's pixels, from
    // the frame presented at `at`; `seq` counts copies, so a late one doesn't
    // undo a newer one. Matched on the tracking thread; the places come back
    // through update().
    void regions_taken(std::vector<Region> regions, std::vector<std::vector<uint8_t>> raw, ToRgb to_rgb,
                       Clock::time_point at, uint64_t seq);
    // The frame size (swapchain pixels), for clipping regions.
    void set_extent(int width, int height) { extent_w_ = width, extent_h_ = height; }

    struct Plate;
    // For the UI: a label above each display in sight.
    struct Label {
        float x = 0, y = 0;  // the plate's top centre (swapchain pixels), predicted for now
        std::string amount;  // "3.2", "45"; or "…" while pending, "?" unpriced
        std::string icon;    // the amount's currency icon: chaos | divine ("" without a price)
        std::string price;   // the same as text: "3.2 div"
        std::string title;   // the item, and what else was read
        std::string note;    // why there's no price
        bool best = false;   // the most valuable one in sight
        bool pending = false;
        // Its plate, for the GPU to find it in the frame (gpu_track.h): the
        // top-left the label was placed for, and the plate's looks.
        float plate_x = 0, plate_y = 0;
        // Where it last was in a copy, before predicting on: after a sharp
        // turn it's between here and plate_x/y.
        float seen_x = 0, seen_y = 0;
        std::shared_ptr<const Plate> plate;
    };
    std::vector<Label> labels() const;
    uint64_t revision() const { return revision_; }

private:
public:
    // A grey template, zero-mean, with its norm.
    struct Template {
        int w = 0, h = 0;
        std::vector<float> v;
        float norm = 0;
    };
    // The plate and the name under it as first seen: tracked by, at
    // quarter, half and full size.
    struct Plate {
        Template t4, t2, t1;
    };
    // One region matched on the tracking thread.
    struct Match {
        uint64_t id = 0, seq = 0;
        Clock::time_point at;
        bool found = false;
        float x = 0, y = 0;   // the plate (full-size pixels)
        float score = 0;      // half size; full size once found
    };

private:
    struct Tracked {
        uint64_t id = 0;
        float x = 0, y = 0, w = 0, h = 0;  // the plate (full-size pixels)
        // What it's tracked by: the plate and the item's name under it (the
        // plates all read "Forbidden Reliquary": alone, they'd let a tag
        // wander to the next display). Its height (full-size pixels).
        float th = 0;
        std::shared_ptr<const Plate> plate;
        // The item's name under the plate (quarter size): how a display seen
        // again is told from the other displays, whose plates are alike.
        Template name;
        int lost = 0;  // copies in a row it wasn't found in
        Clock::time_point seen;
        // Motion, for drawing its label where it is now rather than where it
        // was in the copy (a frame or two ago): pixels a second.
        float vx = 0, vy = 0;
        Clock::time_point sampled;  // when the frame of x, y was presented
        bool moving = false;        // vx, vy are measured
        // Recent places, to measure the speed over a span longer than a
        // frame (a pixel's rounding over 7 ms would be 140 px/s of noise).
        struct Sample {
            Clock::time_point at;
            float x, y;
        };
        std::vector<Sample> samples;
        // Where it was over the last couple of seconds: SCANFIND's plates
        // are from a frame that old, and are matched to where it was then.
        std::vector<Sample> history;
        uint64_t seq = 0;           // the newest copy applied
        std::unique_ptr<DaemonClient> client;
        std::string request;  // SCANITEM, kept to ask again while the daemon loads
        std::future<std::string> request_job;  // making `request`
        Clock::time_point retry_at;
        // The reply is a remembered display's, by the look of its name: read
        // it anyway, and take what that says.
        bool verify = false;
        // Another tracked display shows the same item (that one's tag is
        // shown): kept, so SCANFIND doesn't add it again, but not shown.
        bool duplicate = false;
        bool replied = false;
        PriceReply reply;
    };
    // A display that went out of sight, with its price.
    struct Remembered {
        Template name;
        PriceReply reply;
        Clock::time_point at;
    };

    void start(bool automatic, Clock::time_point now);
    void stop(const char* why);
    void poll_area(Clock::time_point now);
    void poll_find(Clock::time_point now);
    void add_plate(const std::array<float, 8>& p, Clock::time_point now);
    void forget(Tracked& t);
    // One tag per display and per item (vault displays never repeat one).
    void dedupe();
    void apply(const Match& m);
    // The tracking thread: matches regions off the game's thread.
    struct Worker;
    std::unique_ptr<Worker> worker_;
    void changed() { revision_++; }

    bool active_ = false;
    bool automatic_ = false;
    // Turned off by hand in an auto-watch area: stays off until a new area.
    bool suppressed_ = false;
    Clock::time_point started_, last_plate_;
    uint64_t revision_ = 0;
    uint64_t next_id_ = 1;
    Clock::time_point now_;

    // The frame sent to SCANFIND, kept to crop its displays from.
    int frame_w_ = 0, frame_h_ = 0;
    std::vector<uint8_t> frame_;
    bool frame_pending_ = false;
    struct FrameJob {
        std::vector<uint8_t> rgb;
        std::string request;  // SCANFIND; empty if the pixels couldn't be read
    };
    std::future<FrameJob> frame_job_;
    int job_w_ = 0, job_h_ = 0;
    Clock::time_point job_at_, frame_at_;
    Clock::time_point next_find_;
    DaemonClient find_;
    bool find_busy_ = false;

    std::vector<Tracked> tracked_;
    std::vector<Remembered> remembered_;
    int extent_w_ = 0, extent_h_ = 0;

    // AREA, once a second.
    DaemonClient area_client_;
    Clock::time_point next_area_;
    std::string log_hint_;
    long long entered_ = -1;
    bool area_known_ = false;
};
