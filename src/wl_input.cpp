#include "wl_input.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <fcntl.h>
#include <poll.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>

#include <wayland-client.h>

namespace {

// Item text is a few KB; anything far bigger isn't something we want.
constexpr size_t kMaxClipboard = 64 * 1024;

// Preferred first. Wine offers the UTF-8 types for CF_UNICODETEXT.
const char* const kTextMimes[] = {"text/plain;charset=utf-8", "UTF8_STRING", "text/plain"};

int mime_rank(const std::string& mime) {
    for (int i = 0; i < (int)std::size(kTextMimes); i++)
        if (mime == kTextMimes[i]) return i;
    return -1;
}

WaylandInput* self(void* data) { return static_cast<WaylandInput*>(data); }

void reg_global(void* data, wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    self(data)->on_global(reg, name, iface, version);
}
void reg_global_remove(void*, wl_registry*, uint32_t) {}
const wl_registry_listener registry_listener = {reg_global, reg_global_remove};

void seat_caps(void* data, wl_seat*, uint32_t caps) { self(data)->on_seat_caps(caps); }
void seat_name(void*, wl_seat*, const char*) {}
const wl_seat_listener seat_listener = {seat_caps, seat_name};

void kb_keymap(void*, wl_keyboard*, uint32_t, int32_t fd, uint32_t) { close(fd); }
void kb_enter(void* data, wl_keyboard*, uint32_t, wl_surface*, wl_array* keys) {
    self(data)->on_enter(static_cast<uint32_t*>(keys->data), keys->size / sizeof(uint32_t));
}
void kb_leave(void* data, wl_keyboard*, uint32_t, wl_surface*) { self(data)->on_leave(); }
void kb_key(void* data, wl_keyboard*, uint32_t, uint32_t, uint32_t key, uint32_t state) {
    self(data)->on_key(key, state);
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

void offer_mime(void* data, wl_data_offer* offer, const char* mime) { self(data)->on_offer_mime(offer, mime); }
void offer_source_actions(void*, wl_data_offer*, uint32_t) {}
void offer_action(void*, wl_data_offer*, uint32_t) {}
const wl_data_offer_listener offer_listener = {
    .offer = offer_mime,
    .source_actions = offer_source_actions,
    .action = offer_action,
};

void dd_data_offer(void* data, wl_data_device*, wl_data_offer* offer) { self(data)->on_data_offer(offer); }
void dd_enter(void* data, wl_data_device*, uint32_t, wl_surface*, wl_fixed_t, wl_fixed_t, wl_data_offer* offer) {
    self(data)->on_dnd_enter(offer);
}
void dd_leave(void* data, wl_data_device*) { self(data)->on_dnd_leave(); }
void dd_motion(void*, wl_data_device*, uint32_t, wl_fixed_t, wl_fixed_t) {}
void dd_drop(void*, wl_data_device*) {}
void dd_selection(void* data, wl_data_device*, wl_data_offer* offer) { self(data)->on_selection(offer); }
const wl_data_device_listener data_device_listener = {
    .data_offer = dd_data_offer,
    .enter = dd_enter,
    .leave = dd_leave,
    .motion = dd_motion,
    .drop = dd_drop,
    .selection = dd_selection,
};

}  // namespace

WaylandInput::WaylandInput(wl_display* display, wl_surface* parent) : display_(display), parent_(parent) {
    init_scale_policy();
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
    destroy_pointer_objects();
    cancel_receive();
    for (auto& [offer, mime] : offers_) wl_data_offer_destroy(offer);
    if (data_device_) wl_data_device_destroy(data_device_);
    if (data_manager_) wl_data_device_manager_destroy(data_manager_);
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

    continue_receive();
}

const char* WaylandInput::status() const {
    if (keyboard_ && data_device_) return "keyboard + clipboard bound";
    if (keyboard_) return "keyboard bound, no clipboard";
    if (seat_) return "seat has no keyboard";
    return "waiting for seat";
}

void WaylandInput::on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (!seat_ && !strcmp(iface, wl_seat_interface.name)) {
        seat_ = static_cast<wl_seat*>(wl_registry_bind(reg, name, &wl_seat_interface, std::min(version, 5u)));
        wl_seat_add_listener(seat_, &seat_listener, this);
    } else if (!data_manager_ && !strcmp(iface, wl_data_device_manager_interface.name)) {
        data_manager_ = static_cast<wl_data_device_manager*>(
            wl_registry_bind(reg, name, &wl_data_device_manager_interface, std::min(version, 3u)));
    } else {
        bind_pointer_globals(reg, name, iface, version);
    }
    maybe_create_data_device();
}

void WaylandInput::maybe_create_data_device() {
    if (data_device_ || !seat_ || !data_manager_) return;
    data_device_ = wl_data_device_manager_get_data_device(data_manager_, seat_);
    wl_data_device_add_listener(data_device_, &data_device_listener, this);
    poe_log("wayland input: clipboard bound");
}

void WaylandInput::on_seat_caps(uint32_t caps) {
    update_pointer(caps & WL_SEAT_CAPABILITY_POINTER);
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
    if (already_held) return;
    bool alt = held_.count(KEY_LEFTALT) || held_.count(KEY_RIGHTALT);
    if (key == KEY_D && ctrl_held()) {
        hotkeys_.push_back(alt ? Hotkey::PriceCheckLocked : Hotkey::PriceCheck);
        poe_log("wayland input: hotkey Ctrl+%sD", alt ? "Alt+" : "");
    } else if (key == KEY_C && ctrl_held()) {
        hotkeys_.push_back(Hotkey::CopySeen);
    }
}

// ---- clipboard -------------------------------------------------------------

void WaylandInput::on_data_offer(wl_data_offer* offer) {
    offers_[offer] = "";
    wl_data_offer_add_listener(offer, &offer_listener, this);
}

void WaylandInput::on_offer_mime(wl_data_offer* offer, const char* mime) {
    auto it = offers_.find(offer);
    if (it == offers_.end()) return;
    int rank = mime_rank(mime);
    if (rank >= 0 && (it->second.empty() || rank < mime_rank(it->second))) it->second = mime;
}

void WaylandInput::destroy_offer(wl_data_offer* offer) {
    if (!offer) return;
    offers_.erase(offer);
    wl_data_offer_destroy(offer);
}

void WaylandInput::on_dnd_enter(wl_data_offer* offer) {
    if (dnd_offer_ != offer) destroy_offer(dnd_offer_);
    dnd_offer_ = offer;
}

void WaylandInput::on_dnd_leave() {
    destroy_offer(dnd_offer_);
    dnd_offer_ = nullptr;
}

void WaylandInput::on_selection(wl_data_offer* offer) {
    cancel_receive();
    if (selection_ != offer) destroy_offer(selection_);
    selection_ = offer;
    if (offer) start_receive(offer);
}

void WaylandInput::start_receive(wl_data_offer* offer) {
    auto it = offers_.find(offer);
    if (it == offers_.end() || it->second.empty()) return;  // not text

    int fds[2];
    if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        poe_log("clipboard: pipe2 failed: %s", strerror(errno));
        return;
    }
    // The source (Wine) writes into the pipe from its own event handling; we
    // drain it a bit every frame instead of blocking the present thread.
    wl_data_offer_receive(offer, it->second.c_str(), fds[1]);
    close(fds[1]);
    wl_display_flush(display_);
    receive_fd_ = fds[0];
    receive_buf_.clear();
}

void WaylandInput::continue_receive() {
    if (receive_fd_ < 0) return;
    char chunk[4096];
    for (;;) {
        ssize_t n = read(receive_fd_, chunk, sizeof(chunk));
        if (n > 0) {
            receive_buf_.append(chunk, n);
            if (receive_buf_.size() > kMaxClipboard) {
                poe_log("clipboard: over %zu bytes, ignoring", kMaxClipboard);
                cancel_receive();
                return;
            }
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EINTR)) return;  // more later
        break;  // EOF or error
    }
    close(receive_fd_);
    receive_fd_ = -1;
    clipboard_text_ = std::move(receive_buf_);
    receive_buf_.clear();
    clipboard_seq_++;
    poe_log("clipboard: received %zu bytes", clipboard_text_.size());
}

void WaylandInput::cancel_receive() {
    if (receive_fd_ < 0) return;
    close(receive_fd_);
    receive_fd_ = -1;
    receive_buf_.clear();
}
