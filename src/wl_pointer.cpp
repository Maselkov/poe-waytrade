// Mouse input: a transparent, input-only subsurface over the game's surface.
//
// The subsurface is sized to the bounding box of the overlay's panels and
// its input region covers exactly the panels, so the compositor routes
// pointer events there to us instead of the game. With no panel visible it
// is unmapped, which also leaves direct scanout of a fullscreen game alone.
//
// The overlay works in swapchain pixels, Wayland in logical units. With
// fractional scaling the two differ for apps that render at full resolution
// and let the compositor scale down (Wine does), so everything sent to or
// received from the compositor is converted by scale_.
#include "wl_input.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <sys/mman.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <climits>
#include <cmath>
#include <cstdlib>
#include <cstring>

#include <wayland-client.h>
#include <wayland-cursor.h>
#include "fractional-scale-v1-client-protocol.h"

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

void preferred_scale(void* data, wp_fractional_scale_v1*, uint32_t scale_120) {
    self(data)->on_preferred_scale(scale_120);
}
const wp_fractional_scale_v1_listener fractional_scale_listener = {.preferred_scale = preferred_scale};

int round_up(int v) { return (v + kBufferStep - 1) / kBufferStep * kBufferStep; }

}  // namespace

// We can't see the game surface's logical size, so the pixel/logical ratio
// is inferred: Wine renders at the output's full resolution, so the ratio is
// the compositor's fractional scale for our surface (which sits on the same
// output). Apps that render at logical size (e.g. vkcube) have a ratio of 1.
// POE_WAYTRADE_SCALE overrides both.
void WaylandInput::init_scale_policy() {
    if (const char* s = getenv("POE_WAYTRADE_SCALE")) {
        double v = atof(s);
        scale_ = v > 0 ? v : 1.0;
        poe_log("wayland input: scale %.3f (POE_WAYTRADE_SCALE)", scale_);
        return;
    }
    follow_fractional_scale_ = getenv("WINEPREFIX") || getenv("WINELOADER") || getenv("WINEDLLPATH");
}

void WaylandInput::on_preferred_scale(uint32_t scale_120) {
    double s = scale_120 / 120.0;
    if (s <= 0 || s == scale_) return;
    scale_ = s;
    rects_.clear();  // re-place the subsurface next frame
    poe_log("wayland input: scale %.3f (compositor)", scale_);
}

void WaylandInput::bind_pointer_globals(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (!fractional_manager_ && !strcmp(iface, wp_fractional_scale_manager_v1_interface.name)) {
        fractional_manager_ = static_cast<wp_fractional_scale_manager_v1*>(
            wl_registry_bind(reg, name, &wp_fractional_scale_manager_v1_interface, 1));
    } else if (!compositor_ && !strcmp(iface, wl_compositor_interface.name)) {
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
    // Not while a button is held (dragging a panel by its header): moving
    // the subsurface takes effect with the game's next frame, so motion in
    // between would be offset by the old position, and the panel would
    // chase the error. While a button is held, events keep coming to this
    // surface even past its edges; it catches up on release.
    if (buttons_held_ > 0) return;

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
        if (follow_fractional_scale_ && fractional_manager_) {
            fractional_scale_ = wp_fractional_scale_manager_v1_get_fractional_scale(fractional_manager_, input_surface_);
            wp_fractional_scale_v1_add_listener(fractional_scale_, &fractional_scale_listener, this);
        }
        poe_log("wayland input: input subsurface created");
    }

    // Everything below is in logical units.
    auto lo = [this](int px) { return (int)floor(px / scale_); };
    auto hi = [this](int px) { return (int)ceil(px / scale_); };
    width = lo(width);
    height = lo(height);

    int x0 = INT_MAX, y0 = INT_MAX, x1 = INT_MIN, y1 = INT_MIN;
    for (const InputRect& r : rects) {
        x0 = std::min(x0, lo(r.x) - kMargin);
        y0 = std::min(y0, lo(r.y) - kMargin);
        x1 = std::max(x1, hi(r.x + r.w) + kMargin);
        y1 = std::max(y1, hi(r.y + r.h) + kMargin);
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
    for (const InputRect& r : rects) {
        int rx = lo(r.x) - kMargin, ry = lo(r.y) - kMargin;
        wl_region_add(region, rx - x0, ry - y0, hi(r.x + r.w) + kMargin - rx, hi(r.y + r.h) + kMargin - ry);
    }
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
        shm_fd_ = memfd_create("poe-waytrade-input", MFD_CLOEXEC);
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

void WaylandInput::set_cursor(const std::string& name) {
    cursor_ = name;
    if (pointer_inside_ && cursor_ != applied_cursor_) apply_cursor();
}

void WaylandInput::apply_cursor() {
    applied_cursor_ = cursor_;
    if (!pointer_ || !compositor_ || !shm_) return;
    if (!cursor_theme_) {
        const char* size = getenv("XCURSOR_SIZE");
        cursor_theme_ = wl_cursor_theme_load(getenv("XCURSOR_THEME"), size ? atoi(size) : 24, shm_);
        if (!cursor_theme_) return;
    }
    // CSS names first, then the X11 names older themes use.
    std::vector<const char*> names;
    if (cursor_ == "move") names = {"move", "fleur", "grabbing"};
    else if (cursor_ == "pointer") names = {"pointer", "hand2", "pointing_hand"};
    else if (!cursor_.empty()) names = {cursor_.c_str()};
    names.insert(names.end(), {"default", "left_ptr"});

    wl_cursor* cursor = nullptr;
    for (const char* n : names)
        if ((cursor = wl_cursor_theme_get_cursor(cursor_theme_, n))) break;
    if (!cursor || cursor->image_count == 0) return;
    wl_cursor_image* image = cursor->images[0];

    if (!cursor_surface_) cursor_surface_ = wl_compositor_create_surface(compositor_);
    wl_surface_attach(cursor_surface_, wl_cursor_image_get_buffer(image), 0, 0);
    wl_surface_damage(cursor_surface_, 0, 0, image->width, image->height);
    wl_surface_commit(cursor_surface_);
    wl_pointer_set_cursor(pointer_, enter_serial_, cursor_surface_, image->hotspot_x, image->hotspot_y);
    wl_display_flush(display_);
}

void WaylandInput::destroy_pointer_objects() {
    if (cursor_surface_) wl_surface_destroy(cursor_surface_);
    if (cursor_theme_) wl_cursor_theme_destroy(cursor_theme_);
    if (buffer_) wl_buffer_destroy(buffer_);
    if (shm_pool_) wl_shm_pool_destroy(shm_pool_);
    if (shm_fd_ >= 0) close(shm_fd_);
    if (fractional_scale_) wp_fractional_scale_v1_destroy(fractional_scale_);
    if (fractional_manager_) wp_fractional_scale_manager_v1_destroy(fractional_manager_);
    if (input_subsurface_) wl_subsurface_destroy(input_subsurface_);
    if (input_surface_) wl_surface_destroy(input_surface_);
    if (pointer_) wl_pointer_release(pointer_);
    if (shm_) wl_shm_destroy(shm_);
    if (subcompositor_) wl_subcompositor_destroy(subcompositor_);
    if (compositor_) wl_compositor_destroy(compositor_);
}

// ---- pointer events --------------------------------------------------------
// We see pointer events for every surface of the game's connection: ours
// (the panels) and the game's own, which we only watch, to know where the
// cursor is and when the game gets clicked.

bool WaylandInput::game_pointer(float* x, float* y) const {
    if (!game_pointer_known_) return false;
    *x = game_x_;
    *y = game_y_;
    return true;
}

void WaylandInput::on_pointer_enter(uint32_t serial, wl_surface* surface, double x, double y) {
    if (!surface) return;
    if (surface != input_surface_) {
        // The game's surface. Positions are relative to whichever of its
        // surfaces has the pointer; for a fullscreen game that's the one
        // the swapchain covers.
        pointer_on_game_ = true;
        game_pointer_known_ = true;
        game_x_ = float(x * scale_);
        game_y_ = float(y * scale_);
        return;
    }
    pointer_inside_ = true;
    buttons_held_ = 0;  // a release we never saw can't keep the rects frozen
    // The game's cursor doesn't apply over our surface; set our own.
    enter_serial_ = serial;
    apply_cursor();
    pointer_events_.push_back({PointerEvent::Motion, float((x + sub_x_) * scale_), float((y + sub_y_) * scale_)});
}

void WaylandInput::on_pointer_leave(wl_surface* surface) {
    if (surface != input_surface_) {
        pointer_on_game_ = false;
        return;
    }
    buttons_held_ = 0;
    if (!pointer_inside_) return;
    pointer_inside_ = false;
    pointer_events_.push_back({PointerEvent::Leave});
}

void WaylandInput::on_pointer_motion(double x, double y) {
    if (pointer_inside_) {
        pointer_events_.push_back({PointerEvent::Motion, float((x + sub_x_) * scale_), float((y + sub_y_) * scale_)});
    } else if (pointer_on_game_) {
        game_x_ = float(x * scale_);
        game_y_ = float(y * scale_);
    }
}

void WaylandInput::on_pointer_button(uint32_t button, uint32_t state) {
    bool pressed = state == WL_POINTER_BUTTON_STATE_PRESSED;
    if (!pointer_inside_) {
        if (pointer_on_game_ && pressed) game_clicks_++;
        return;
    }
    buttons_held_ = std::max(0, buttons_held_ + (pressed ? 1 : -1));
    int index = button == BTN_LEFT ? 0 : button == BTN_RIGHT ? 1 : button == BTN_MIDDLE ? 2 : -1;
    if (index < 0) return;
    PointerEvent ev{PointerEvent::Button};
    ev.button = index;
    ev.down = pressed;
    pointer_events_.push_back(ev);
}

void WaylandInput::on_pointer_axis(uint32_t axis, double value) {
    if (!pointer_inside_ || axis != WL_POINTER_AXIS_VERTICAL_SCROLL) return;
    PointerEvent ev{PointerEvent::Wheel};
    ev.wheel = float(-value / 10.0);  // ~10 units per wheel notch
    pointer_events_.push_back(ev);
}
