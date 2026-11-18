// Mouse input: a transparent, input-only subsurface over the game's surface.
//
// The subsurface is sized to the bounding box of the overlay's panels and
// its input region covers exactly the panels, so the compositor routes
// pointer events there to us instead of the game. With no panel visible it
// is unmapped, which also leaves direct scanout of a fullscreen game alone.
#include "wl_input.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <sys/mman.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <climits>
#include <cstring>

#include <wayland-client.h>

namespace {

// Extra input area around each panel, so a quick drag or a click on the
// border doesn't slip through to the game.
constexpr int kMargin = 16;
// Round the buffer up so it isn't reallocated for every pixel of resize.
constexpr int kBufferStep = 64;

WaylandInput* self(void* data) { return static_cast<WaylandInput*>(data); }

void ptr_enter(void* data, wl_pointer*, uint32_t serial, wl_surface* surface, wl_fixed_t x, wl_fixed_t y) {
    self(data)->on_pointer_enter(serial, surface, wl_fixed_to_double(x), wl_fixed_to_double(y));
}
void ptr_leave(void* data, wl_pointer*, uint32_t, wl_surface* surface) { self(data)->on_pointer_leave(surface); }
void ptr_motion(void* data, wl_pointer*, uint32_t, wl_fixed_t x, wl_fixed_t y) {
    self(data)->on_pointer_motion(wl_fixed_to_double(x), wl_fixed_to_double(y));
}
void ptr_button(void* data, wl_pointer*, uint32_t, uint32_t, uint32_t button, uint32_t state) {
    self(data)->on_pointer_button(button, state);
}
void ptr_axis(void* data, wl_pointer*, uint32_t, uint32_t axis, wl_fixed_t value) {
    self(data)->on_pointer_axis(axis, wl_fixed_to_double(value));
}
void ptr_frame(void*, wl_pointer*) {}
void ptr_axis_source(void*, wl_pointer*, uint32_t) {}
void ptr_axis_stop(void*, wl_pointer*, uint32_t, uint32_t) {}
void ptr_axis_discrete(void*, wl_pointer*, uint32_t, int32_t) {}
const wl_pointer_listener pointer_listener = {
    .enter = ptr_enter,
    .leave = ptr_leave,
    .motion = ptr_motion,
    .button = ptr_button,
    .axis = ptr_axis,
    .frame = ptr_frame,
    .axis_source = ptr_axis_source,
    .axis_stop = ptr_axis_stop,
    .axis_discrete = ptr_axis_discrete,
};

int round_up(int v) { return (v + kBufferStep - 1) / kBufferStep * kBufferStep; }

}  // namespace

void WaylandInput::bind_pointer_globals(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (!compositor_ && !strcmp(iface, wl_compositor_interface.name)) {
        compositor_ = static_cast<wl_compositor*>(
            wl_registry_bind(reg, name, &wl_compositor_interface, std::min(version, 4u)));
    } else if (!subcompositor_ && !strcmp(iface, wl_subcompositor_interface.name)) {
        subcompositor_ = static_cast<wl_subcompositor*>(wl_registry_bind(reg, name, &wl_subcompositor_interface, 1));
    } else if (!shm_ && !strcmp(iface, wl_shm_interface.name)) {
        shm_ = static_cast<wl_shm*>(wl_registry_bind(reg, name, &wl_shm_interface, 1));
    }
}

void WaylandInput::update_pointer(bool has_pointer) {
    if (has_pointer && !pointer_) {
        pointer_ = wl_seat_get_pointer(seat_);
        wl_pointer_add_listener(pointer_, &pointer_listener, this);
        poe_log("wayland input: pointer bound");
    } else if (!has_pointer && pointer_) {
        wl_pointer_release(pointer_);
        pointer_ = nullptr;
        pointer_inside_ = false;
    }
}

void WaylandInput::set_input_rects(const std::vector<InputRect>& rects, int width, int height) {
    if (rects == rects_) return;

    if (rects.empty()) {
        rects_.clear();
        if (mapped_) {
            wl_surface_attach(input_surface_, nullptr, 0, 0);
            wl_surface_commit(input_surface_);
            wl_display_flush(display_);
            mapped_ = false;
        }
        return;
    }
    if (!compositor_ || !subcompositor_ || !shm_ || !parent_) return;  // retried next frame
    rects_ = rects;

    if (!input_surface_) {
        input_surface_ = wl_compositor_create_surface(compositor_);
        input_subsurface_ = wl_subcompositor_get_subsurface(subcompositor_, input_surface_, parent_);
        // Apply our commits right away instead of with the game's next one.
        wl_subsurface_set_desync(input_subsurface_);
        poe_log("wayland input: input subsurface created");
    }

    int x0 = INT_MAX, y0 = INT_MAX, x1 = INT_MIN, y1 = INT_MIN;
    for (const InputRect& r : rects) {
        x0 = std::min(x0, r.x - kMargin);
        y0 = std::min(y0, r.y - kMargin);
        x1 = std::max(x1, r.x + r.w + kMargin);
        y1 = std::max(y1, r.y + r.h + kMargin);
    }
    x0 = std::max(x0, 0);
    y0 = std::max(y0, 0);
    x1 = std::min(x1, width);
    y1 = std::min(y1, height);
    if (x1 <= x0 || y1 <= y0) return;

    bool new_buffer = ensure_buffer(std::min(round_up(x1 - x0), width - x0),
                                    std::min(round_up(y1 - y0), height - y0));
    if (x0 != sub_x_ || y0 != sub_y_ || !mapped_) {
        // Takes effect with the game's next commit, i.e. its next frame.
        wl_subsurface_set_position(input_subsurface_, x0, y0);
        sub_x_ = x0;
        sub_y_ = y0;
    }

    wl_region* region = wl_compositor_create_region(compositor_);
    for (const InputRect& r : rects)
        wl_region_add(region, r.x - kMargin - x0, r.y - kMargin - y0, r.w + 2 * kMargin, r.h + 2 * kMargin);
    wl_surface_set_input_region(input_surface_, region);
    wl_region_destroy(region);

    if (new_buffer || !mapped_) {
        wl_surface_attach(input_surface_, buffer_, 0, 0);
        wl_surface_damage(input_surface_, 0, 0, buffer_w_, buffer_h_);
    }
    wl_surface_commit(input_surface_);
    wl_display_flush(display_);
    mapped_ = true;
}

// A fully transparent ARGB buffer (memfd pages read as zero). Returns true
// if a new wl_buffer was created and needs attaching.
bool WaylandInput::ensure_buffer(int w, int h) {
    if (buffer_ && w == buffer_w_ && h == buffer_h_) return false;

    size_t size = (size_t)w * h * 4;
    if (shm_fd_ < 0) {
        shm_fd_ = memfd_create("poe-overlay-input", MFD_CLOEXEC);
        if (shm_fd_ < 0 || ftruncate(shm_fd_, size) != 0) {
            poe_log("wayland input: shm buffer: %s", strerror(errno));
            return false;
        }
        shm_pool_ = wl_shm_create_pool(shm_, shm_fd_, size);
        shm_size_ = size;
    } else if (size > shm_size_) {
        if (ftruncate(shm_fd_, size) != 0) return false;
        wl_shm_pool_resize(shm_pool_, size);
        shm_size_ = size;
    }

    // The old buffer may still be attached; shm contents are copied at
    // commit, so destroying it right away is fine.
    if (buffer_) wl_buffer_destroy(buffer_);
    buffer_ = wl_shm_pool_create_buffer(shm_pool_, 0, w, h, w * 4, WL_SHM_FORMAT_ARGB8888);
    buffer_w_ = w;
    buffer_h_ = h;
    return true;
}

void WaylandInput::destroy_pointer_objects() {
    if (buffer_) wl_buffer_destroy(buffer_);
    if (shm_pool_) wl_shm_pool_destroy(shm_pool_);
    if (shm_fd_ >= 0) close(shm_fd_);
    if (input_subsurface_) wl_subsurface_destroy(input_subsurface_);
    if (input_surface_) wl_surface_destroy(input_surface_);
    if (pointer_) wl_pointer_release(pointer_);
    if (shm_) wl_shm_destroy(shm_);
    if (subcompositor_) wl_subcompositor_destroy(subcompositor_);
    if (compositor_) wl_compositor_destroy(compositor_);
}

// ---- pointer events --------------------------------------------------------
// We see pointer events for every surface of the game's connection; only the
// ones on our subsurface are ours.

void WaylandInput::on_pointer_enter(uint32_t serial, wl_surface* surface, double x, double y) {
    if (!surface || surface != input_surface_) return;
    pointer_inside_ = true;
    // Hide the compositor cursor here; ImGui draws its own while inside.
    wl_pointer_set_cursor(pointer_, serial, nullptr, 0, 0);
    pointer_events_.push_back({PointerEvent::Motion, float(x + sub_x_), float(y + sub_y_)});
}

void WaylandInput::on_pointer_leave(wl_surface* surface) {
    if (!pointer_inside_ || surface != input_surface_) return;
    pointer_inside_ = false;
    pointer_events_.push_back({PointerEvent::Leave});
}

void WaylandInput::on_pointer_motion(double x, double y) {
    if (pointer_inside_) pointer_events_.push_back({PointerEvent::Motion, float(x + sub_x_), float(y + sub_y_)});
}

void WaylandInput::on_pointer_button(uint32_t button, uint32_t state) {
    if (!pointer_inside_) return;
    int index = button == BTN_LEFT ? 0 : button == BTN_RIGHT ? 1 : button == BTN_MIDDLE ? 2 : -1;
    if (index < 0) return;
    PointerEvent ev{PointerEvent::Button};
    ev.button = index;
    ev.down = state == WL_POINTER_BUTTON_STATE_PRESSED;
    pointer_events_.push_back(ev);
}

void WaylandInput::on_pointer_axis(uint32_t axis, double value) {
    if (!pointer_inside_ || axis != WL_POINTER_AXIS_VERTICAL_SCROLL) return;
    PointerEvent ev{PointerEvent::Wheel};
    ev.wheel = float(-value / 10.0);  // ~10 units per wheel notch
    pointer_events_.push_back(ev);
}
