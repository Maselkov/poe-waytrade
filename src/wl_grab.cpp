#include "wl_grab.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <poll.h>
#include <sys/mman.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>

#include <wayland-client.h>
#include "wlr-layer-shell-unstable-v1-client-protocol.h"

namespace {

void registry_global(void* data, wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    static_cast<KeyboardGrab*>(data)->on_global(reg, name, iface, version);
}
void registry_global_remove(void*, wl_registry*, uint32_t) {}
const wl_registry_listener registry_listener = {registry_global, registry_global_remove};

void seat_caps(void* data, wl_seat*, uint32_t caps) { static_cast<KeyboardGrab*>(data)->on_seat_caps(caps); }
void seat_name(void*, wl_seat*, const char*) {}
const wl_seat_listener seat_listener = {seat_caps, seat_name};

void kb_keymap(void*, wl_keyboard*, uint32_t, int32_t fd, uint32_t) { close(fd); }
void kb_enter(void* data, wl_keyboard*, uint32_t, wl_surface*, wl_array*) {
    static_cast<KeyboardGrab*>(data)->on_enter();
}
void kb_leave(void* data, wl_keyboard*, uint32_t, wl_surface*) { static_cast<KeyboardGrab*>(data)->on_leave(); }
void kb_key(void* data, wl_keyboard*, uint32_t, uint32_t, uint32_t key, uint32_t state) {
    static_cast<KeyboardGrab*>(data)->on_key(key, state);
}
void kb_modifiers(void*, wl_keyboard*, uint32_t, uint32_t, uint32_t, uint32_t, uint32_t) {}
void kb_repeat(void*, wl_keyboard*, int32_t, int32_t) {}
const wl_keyboard_listener keyboard_listener = {kb_keymap, kb_enter, kb_leave, kb_key, kb_modifiers, kb_repeat};

void layer_configure(void* data, zwlr_layer_surface_v1*, uint32_t serial, uint32_t, uint32_t) {
    static_cast<KeyboardGrab*>(data)->on_configure(serial);
}
void layer_closed(void* data, zwlr_layer_surface_v1*) { static_cast<KeyboardGrab*>(data)->on_closed(); }
const zwlr_layer_surface_v1_listener layer_listener = {layer_configure, layer_closed};

}  // namespace

KeyboardGrab::~KeyboardGrab() {
    release();
    if (keyboard_) wl_keyboard_destroy(keyboard_);
    if (seat_) wl_seat_destroy(seat_);
    if (layer_shell_) zwlr_layer_shell_v1_destroy(layer_shell_);
    if (shm_) wl_shm_destroy(shm_);
    if (compositor_) wl_compositor_destroy(compositor_);
    if (registry_) wl_registry_destroy(registry_);
    if (display_) wl_display_disconnect(display_);
}

bool KeyboardGrab::supported() {
    if (!tried_) {
        tried_ = true;
        if (!connect()) poe_log("keyboard grab: unavailable (no layer-shell): typing in panels is off");
    }
    return layer_shell_ && compositor_ && shm_ && keyboard_;
}

bool KeyboardGrab::connect() {
    // WAYLAND_DISPLAY, as the game (Wine) connects.
    display_ = wl_display_connect(nullptr);
    if (!display_) return false;
    registry_ = wl_display_get_registry(display_);
    wl_registry_add_listener(registry_, &registry_listener, this);
    wl_display_roundtrip(display_);  // globals
    wl_display_roundtrip(display_);  // seat capabilities -> keyboard
    return layer_shell_ && keyboard_;
}

void KeyboardGrab::on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (!compositor_ && !strcmp(iface, wl_compositor_interface.name)) {
        compositor_ = static_cast<wl_compositor*>(wl_registry_bind(reg, name, &wl_compositor_interface, 4));
    } else if (!shm_ && !strcmp(iface, wl_shm_interface.name)) {
        shm_ = static_cast<wl_shm*>(wl_registry_bind(reg, name, &wl_shm_interface, 1));
    } else if (!seat_ && !strcmp(iface, wl_seat_interface.name)) {
        seat_ = static_cast<wl_seat*>(wl_registry_bind(reg, name, &wl_seat_interface, std::min(version, 5u)));
        wl_seat_add_listener(seat_, &seat_listener, this);
    } else if (!layer_shell_ && !strcmp(iface, zwlr_layer_shell_v1_interface.name)) {
        layer_shell_ = static_cast<zwlr_layer_shell_v1*>(
            wl_registry_bind(reg, name, &zwlr_layer_shell_v1_interface, std::min(version, 4u)));
    }
}

void KeyboardGrab::on_seat_caps(uint32_t caps) {
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !keyboard_) {
        keyboard_ = wl_seat_get_keyboard(seat_);
        wl_keyboard_add_listener(keyboard_, &keyboard_listener, this);
    }
}

void KeyboardGrab::take() {
    if (surface_ || !supported()) return;
    surface_ = wl_compositor_create_surface(compositor_);
    // Clicks go through to whatever is below (the panels' input subsurface,
    // or the game).
    wl_region* none = wl_compositor_create_region(compositor_);
    wl_surface_set_input_region(surface_, none);
    wl_region_destroy(none);
    layer_surface_ = zwlr_layer_shell_v1_get_layer_surface(layer_shell_, surface_, nullptr,
                                                           ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY, "poe-waytrade");
    zwlr_layer_surface_v1_add_listener(layer_surface_, &layer_listener, this);
    zwlr_layer_surface_v1_set_size(layer_surface_, 1, 1);
    zwlr_layer_surface_v1_set_anchor(layer_surface_,
                                     ZWLR_LAYER_SURFACE_V1_ANCHOR_TOP | ZWLR_LAYER_SURFACE_V1_ANCHOR_LEFT);
    zwlr_layer_surface_v1_set_exclusive_zone(layer_surface_, -1);
    // 1 is "exclusive" in every version (a plain boolean before v4).
    zwlr_layer_surface_v1_set_keyboard_interactivity(layer_surface_, 1);
    wl_surface_commit(surface_);
    wl_display_flush(display_);
    keys_.clear();
    poe_log("keyboard grab: taking the keyboard");
}

void KeyboardGrab::release() {
    if (!surface_) return;
    if (buffer_) wl_buffer_destroy(buffer_);
    if (layer_surface_) zwlr_layer_surface_v1_destroy(layer_surface_);
    wl_surface_destroy(surface_);
    buffer_ = nullptr;
    layer_surface_ = nullptr;
    surface_ = nullptr;
    focused_ = false;
    shift_ = false;
    keys_.clear();
    if (display_) wl_display_flush(display_);
    poe_log("keyboard grab: keyboard given back");
}

void KeyboardGrab::on_configure(uint32_t serial) {
    if (!layer_surface_) return;
    zwlr_layer_surface_v1_ack_configure(layer_surface_, serial);
    if (!buffer_) attach_buffer();
    wl_surface_commit(surface_);
}

// The compositor took the surface away (e.g. the output went): typing ends.
void KeyboardGrab::on_closed() { release(); }

// One transparent pixel: a layer surface has to show something to be mapped.
void KeyboardGrab::attach_buffer() {
    int fd = memfd_create("poe-waytrade-grab", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, 4) != 0) {
        poe_log("keyboard grab: shm buffer: %s", strerror(errno));
        if (fd >= 0) close(fd);
        return;
    }
    wl_shm_pool* pool = wl_shm_create_pool(shm_, fd, 4);
    buffer_ = wl_shm_pool_create_buffer(pool, 0, 1, 1, 4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    wl_surface_attach(surface_, buffer_, 0, 0);
}

void KeyboardGrab::on_enter() {
    focused_ = true;
    poe_log("keyboard grab: we have the keyboard");
}

void KeyboardGrab::on_leave() {
    focused_ = false;
    if (surface_) poe_log("keyboard grab: the compositor took the keyboard back");
}

void KeyboardGrab::on_key(uint32_t key, uint32_t state) {
    bool down = state == WL_KEYBOARD_KEY_STATE_PRESSED;
    if (key == KEY_LEFTSHIFT || key == KEY_RIGHTSHIFT) {
        shift_ = down;
        return;
    }
    if (down && surface_) keys_.push_back({key, shift_});
}

void KeyboardGrab::poll() {
    if (!display_) return;
    // Only this thread uses this connection, but the read is done the
    // standard way anyway.
    while (wl_display_prepare_read(display_) != 0) wl_display_dispatch_pending(display_);
    wl_display_flush(display_);
    pollfd pfd = {wl_display_get_fd(display_), POLLIN, 0};
    if (::poll(&pfd, 1, 0) > 0)
        wl_display_read_events(display_);
    else
        wl_display_cancel_read(display_);
    wl_display_dispatch_pending(display_);
}
