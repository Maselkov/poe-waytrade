#include "wl_input.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <poll.h>
#include <unistd.h>

#include <algorithm>
#include <cstring>

#include <wayland-client.h>

namespace {

void reg_global(void* data, wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    static_cast<WaylandInput*>(data)->on_global(reg, name, iface, version);
}
void reg_global_remove(void*, wl_registry*, uint32_t) {}
const wl_registry_listener registry_listener = {reg_global, reg_global_remove};

void seat_caps(void* data, wl_seat*, uint32_t caps) {
    static_cast<WaylandInput*>(data)->on_seat_caps(caps);
}
void seat_name(void*, wl_seat*, const char*) {}
const wl_seat_listener seat_listener = {seat_caps, seat_name};

void kb_keymap(void*, wl_keyboard*, uint32_t, int32_t fd, uint32_t) { close(fd); }
void kb_enter(void* data, wl_keyboard*, uint32_t, wl_surface*, wl_array* keys) {
    static_cast<WaylandInput*>(data)->on_enter(static_cast<uint32_t*>(keys->data),
                                               keys->size / sizeof(uint32_t));
}
void kb_leave(void* data, wl_keyboard*, uint32_t, wl_surface*) {
    static_cast<WaylandInput*>(data)->on_leave();
}
void kb_key(void* data, wl_keyboard*, uint32_t, uint32_t, uint32_t key, uint32_t state) {
    static_cast<WaylandInput*>(data)->on_key(key, state);
}
void kb_modifiers(void*, wl_keyboard*, uint32_t, uint32_t, uint32_t, uint32_t, uint32_t) {}
void kb_repeat_info(void*, wl_keyboard*, int32_t, int32_t) {}
const wl_keyboard_listener keyboard_listener = {
    .keymap = kb_keymap,
    .enter = kb_enter,
    .leave = kb_leave,
    .key = kb_key,
    .modifiers = kb_modifiers,
    .repeat_info = kb_repeat_info,
};

}  // namespace

WaylandInput::WaylandInput(wl_display* display) : display_(display) {
    queue_ = wl_display_create_queue(display_);
    // Requests made through the wrapper put their new objects on our queue,
    // so the app's own event dispatching never sees them.
    auto* wrapper = static_cast<wl_display*>(wl_proxy_create_wrapper(display_));
    wl_proxy_set_queue(reinterpret_cast<wl_proxy*>(wrapper), queue_);
    registry_ = wl_display_get_registry(wrapper);
    wl_proxy_wrapper_destroy(wrapper);
    wl_registry_add_listener(registry_, &registry_listener, this);
    wl_display_flush(display_);
    poe_log("wayland input: listening on app's wl_display %p", (void*)display_);
}

WaylandInput::~WaylandInput() {
    if (keyboard_) wl_keyboard_destroy(keyboard_);
    if (seat_) wl_seat_destroy(seat_);
    if (registry_) wl_registry_destroy(registry_);
    if (queue_) wl_event_queue_destroy(queue_);
}

void WaylandInput::poll() {
    // Standard multi-reader pattern: the app's thread reads the same socket,
    // libwayland routes each event to the queue that owns its object.
    while (wl_display_prepare_read_queue(display_, queue_) != 0)
        wl_display_dispatch_queue_pending(display_, queue_);
    wl_display_flush(display_);

    pollfd pfd = {wl_display_get_fd(display_), POLLIN, 0};
    if (::poll(&pfd, 1, 0) > 0)
        wl_display_read_events(display_);
    else
        wl_display_cancel_read(display_);
    wl_display_dispatch_queue_pending(display_, queue_);
}

const char* WaylandInput::status() const {
    if (keyboard_) return "keyboard bound";
    if (seat_) return "seat has no keyboard";
    return "waiting for seat";
}

void WaylandInput::on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (seat_ || strcmp(iface, wl_seat_interface.name) != 0) return;
    seat_ = static_cast<wl_seat*>(wl_registry_bind(reg, name, &wl_seat_interface, std::min(version, 5u)));
    wl_seat_add_listener(seat_, &seat_listener, this);
}

void WaylandInput::on_seat_caps(uint32_t caps) {
    bool has_kb = caps & WL_SEAT_CAPABILITY_KEYBOARD;
    if (has_kb && !keyboard_) {
        keyboard_ = wl_seat_get_keyboard(seat_);
        wl_keyboard_add_listener(keyboard_, &keyboard_listener, this);
        poe_log("wayland input: keyboard bound");
    } else if (!has_kb && keyboard_) {
        wl_keyboard_release(keyboard_);
        keyboard_ = nullptr;
        on_leave();
    }
}

void WaylandInput::on_enter(const uint32_t* keys, size_t count) {
    focused_ = true;
    held_.clear();
    held_.insert(keys, keys + count);
}

void WaylandInput::on_leave() {
    focused_ = false;
    held_.clear();
}

bool WaylandInput::ctrl_held() const {
    return held_.count(KEY_LEFTCTRL) || held_.count(KEY_RIGHTCTRL);
}

void WaylandInput::on_key(uint32_t key, uint32_t state) {
    bool down = state == WL_KEYBOARD_KEY_STATE_PRESSED;
    last_key_ = key;
    last_key_down_ = down;
    if (!down) {
        held_.erase(key);
        return;
    }
    // Key repeat (wl_keyboard v10) re-sends "repeated" rather than "pressed";
    // we only bind v5, so every press here is a fresh one.
    bool already_held = held_.count(key);
    held_.insert(key);
    if (key == KEY_D && ctrl_held() && !already_held) {
        hotkey_presses_++;
        poe_log("wayland input: hotkey Ctrl+D");
    }
}
