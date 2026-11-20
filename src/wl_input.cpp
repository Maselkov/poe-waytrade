#include "wl_input.h"
#include "layer.h"

#include <linux/input-event-codes.h>
#include <fcntl.h>
#include <poll.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>
#include <csignal>
#include <ctime>

#include <pthread.h>

#include <wayland-client.h>

#include "ext-data-control-v1-client-protocol.h"

namespace {

// Item text is a few KB; anything far bigger isn't something we want.
constexpr size_t kMaxClipboard = 64 * 1024;
// The user's clipboard, saved around our copy: images can be big, but not
// unboundedly so.
constexpr size_t kMaxSaved = 32 * 1024 * 1024;
constexpr size_t kMaxSavedMimes = 32;
// Give up restoring if the save never finishes.
constexpr uint64_t kRestoreTimeoutMs = 3000;

uint64_t now_ms() {
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

// write() to a pipe whose reader is gone raises SIGPIPE, which would kill
// the game: block it around the write and swallow one that became pending.
ssize_t write_no_sigpipe(int fd, const char* data, size_t size) {
    sigset_t pipe_set, old;
    sigemptyset(&pipe_set);
    sigaddset(&pipe_set, SIGPIPE);
    pthread_sigmask(SIG_BLOCK, &pipe_set, &old);
    ssize_t n = write(fd, data, size);
    if (n < 0 && errno == EPIPE) {
        int saved = errno;
        timespec zero = {0, 0};
        sigtimedwait(&pipe_set, nullptr, &zero);
        errno = saved;
    }
    pthread_sigmask(SIG_SETMASK, &old, nullptr);
    return n;
}

// X11 selection targets that XWayland/Wine may list; they aren't data.
bool junk_mime(const std::string& mime) {
    static const char* const junk[] = {"TARGETS", "MULTIPLE", "TIMESTAMP", "SAVE_TARGETS", "DELETE",
                                       "INSERT_PROPERTY", "INSERT_SELECTION"};
    for (const char* j : junk)
        if (mime == j) return true;
    return false;
}

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
void kb_enter(void* data, wl_keyboard*, uint32_t serial, wl_surface*, wl_array* keys) {
    self(data)->on_input_serial(serial);
    self(data)->on_enter(static_cast<uint32_t*>(keys->data), keys->size / sizeof(uint32_t));
}
void kb_leave(void* data, wl_keyboard*, uint32_t, wl_surface*) { self(data)->on_leave(); }
void kb_key(void* data, wl_keyboard*, uint32_t serial, uint32_t, uint32_t key, uint32_t state) {
    self(data)->on_input_serial(serial);
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
void src_target(void*, wl_data_source*, const char*) {}
void src_send(void* data, wl_data_source* source, const char* mime, int32_t fd) {
    self(data)->on_source_send(source, mime, fd);
}
void src_cancelled(void* data, wl_data_source* source) { self(data)->on_source_cancelled(source); }
void src_dnd_drop_performed(void*, wl_data_source*) {}
void src_dnd_finished(void*, wl_data_source*) {}
void src_action(void*, wl_data_source*, uint32_t) {}
const wl_data_source_listener source_listener = {
    .target = src_target,
    .send = src_send,
    .cancelled = src_cancelled,
    .dnd_drop_performed = src_dnd_drop_performed,
    .dnd_finished = src_dnd_finished,
    .action = src_action,
};

void ctl_src_send(void* data, ext_data_control_source_v1* source, const char* mime, int32_t fd) {
    self(data)->on_control_send(source, mime, fd);
}
void ctl_src_cancelled(void* data, ext_data_control_source_v1* source) { self(data)->on_control_cancelled(source); }
const ext_data_control_source_v1_listener control_source_listener = {
    .send = ctl_src_send,
    .cancelled = ctl_src_cancelled,
};

// We only write the clipboard through data-control (reading goes through
// the data device, like the game's), so offers are dropped right away.
void ctl_dev_data_offer(void*, ext_data_control_device_v1*, ext_data_control_offer_v1*) {}
void ctl_dev_selection(void*, ext_data_control_device_v1*, ext_data_control_offer_v1* offer) {
    if (offer) ext_data_control_offer_v1_destroy(offer);
}
void ctl_dev_finished(void* data, ext_data_control_device_v1*) { self(data)->on_control_finished(); }
void ctl_dev_primary_selection(void*, ext_data_control_device_v1*, ext_data_control_offer_v1* offer) {
    if (offer) ext_data_control_offer_v1_destroy(offer);
}
const ext_data_control_device_v1_listener control_device_listener = {
    .data_offer = ctl_dev_data_offer,
    .selection = ctl_dev_selection,
    .finished = ctl_dev_finished,
    .primary_selection = ctl_dev_primary_selection,
};

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
    clear_saved();
    for (PendingWrite& w : writes_) close(w.fd);
    if (own_source_) wl_data_source_destroy(own_source_);
    if (own_control_source_) ext_data_control_source_v1_destroy(own_control_source_);
    if (control_device_) ext_data_control_device_v1_destroy(control_device_);
    if (control_manager_) ext_data_control_manager_v1_destroy(control_manager_);
    for (auto& [offer, info] : offers_) wl_data_offer_destroy(offer);
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
    continue_save();
    if (restore_wanted_ && save_state_ == SaveState::Ready) do_restore();
    if (restore_wanted_ && now_ms() > restore_deadline_ms_) {
        poe_log("clipboard: saving took too long, not restoring");
        restore_wanted_ = false;
        clear_saved();
    }
    continue_writes();
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
    } else if (!control_manager_ && !strcmp(iface, ext_data_control_manager_v1_interface.name)) {
        control_manager_ = static_cast<ext_data_control_manager_v1*>(
            wl_registry_bind(reg, name, &ext_data_control_manager_v1_interface, 1));
    } else {
        bind_pointer_globals(reg, name, iface, version);
    }
    maybe_create_data_device();
}

void WaylandInput::maybe_create_data_device() {
    // Globals arrive in any order: data-control may come after the rest.
    if (seat_ && control_manager_ && !control_device_) {
        control_device_ = ext_data_control_manager_v1_get_data_device(control_manager_, seat_);
        ext_data_control_device_v1_add_listener(control_device_, &control_device_listener, this);
    }
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

bool WaylandInput::alt_held() const {
    return held_.count(KEY_LEFTALT) || held_.count(KEY_RIGHTALT);
}

bool WaylandInput::ctrl_held() const {
    return held_.count(KEY_LEFTCTRL) || held_.count(KEY_RIGHTCTRL);
}

uint32_t WaylandInput::mods() const {
    uint32_t m = 0;
    for (uint32_t key : held_) m |= key_mod(key);
    return m;
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
    if (key_mod(key)) return;
    uint32_t held = mods();
    if (capturing_) {
        capturing_ = false;
        captured_ = KeyCombo{held, key};
        poe_log("wayland input: captured %s", combo_label(*captured_).c_str());
        return;
    }
    if (key == KEY_C && (held & kModCtrl)) {
        hotkeys_.push_back(Hotkey::CopySeen);
        return;
    }
    // Exactly the binding's modifiers; failing that, one without Shift and
    // Super matches with them held too (Ctrl+Shift+D is still Ctrl+D).
    const HotkeyBinding* match = nullptr;
    for (const HotkeyBinding& b : bindings_)
        if (!match && b.combo.code == key && b.combo.mods == held) match = &b;
    uint32_t extra = kModShift | kModSuper;
    for (const HotkeyBinding& b : bindings_)
        if (!match && b.combo.code == key && !(b.combo.mods & extra) && b.combo.mods == (held & ~extra)) match = &b;
    if (match) {
        hotkeys_.push_back(match->hotkey);
        poe_log("wayland input: hotkey %s", combo_label(match->combo).c_str());
    }
}

// ---- clipboard -------------------------------------------------------------

void WaylandInput::on_data_offer(wl_data_offer* offer) {
    offers_[offer] = {};
    wl_data_offer_add_listener(offer, &offer_listener, this);
}

void WaylandInput::on_offer_mime(wl_data_offer* offer, const char* mime) {
    auto it = offers_.find(offer);
    if (it == offers_.end()) return;
    it->second.mimes.push_back(mime);
    int rank = mime_rank(mime);
    std::string& best = it->second.text_mime;
    if (rank >= 0 && (best.empty() || rank < mime_rank(best))) best = mime;
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
    // Our restored clipboard coming back to us: not a copy.
    if (offer && expect_own_selection_ && own_active()) {
        expect_own_selection_ = false;
        return;
    }
    if (offer) start_receive(offer);
}

void WaylandInput::start_receive(wl_data_offer* offer) {
    auto it = offers_.find(offer);
    if (it == offers_.end() || it->second.text_mime.empty()) return;  // not text

    int fds[2];
    if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) {
        poe_log("clipboard: pipe2 failed: %s", strerror(errno));
        return;
    }
    // The source (Wine) writes into the pipe from its own event handling; we
    // drain it a bit every frame instead of blocking the present thread.
    wl_data_offer_receive(offer, it->second.text_mime.c_str(), fds[1]);
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

// ---- keeping the user's clipboard ------------------------------------------
//
// Ctrl+D makes the game copy the item, which replaces the clipboard. So
// before that we read everything the clipboard offers (every mime type, all
// at once over pipes, drained each frame), and once the item text has
// arrived we become the clipboard owner with that saved content. Like
// `wl-copy`, the content lives in this process: it stays the clipboard until
// something else is copied, or the game exits (a clipboard manager such as
// Klipper usually keeps it then).

void WaylandInput::clear_saved() {
    for (SavedMime& m : saved_)
        if (m.fd >= 0) close(m.fd);
    saved_.clear();
    save_state_ = SaveState::None;
}

void WaylandInput::save_selection() {
    restore_wanted_ = false;
    // Still serving an earlier restore: that saved content is current.
    if (own_active() && !saved_.empty()) {
        save_state_ = SaveState::Ready;
        return;
    }
    clear_saved();
    if (!data_device_ || !selection_) return;  // nothing to keep
    auto it = offers_.find(selection_);
    if (it == offers_.end()) return;
    for (const std::string& mime : it->second.mimes) {
        if (junk_mime(mime) || saved_.size() >= kMaxSavedMimes) continue;
        if (std::any_of(saved_.begin(), saved_.end(), [&](const SavedMime& m) { return m.mime == mime; })) continue;
        int fds[2];
        if (pipe2(fds, O_CLOEXEC | O_NONBLOCK) != 0) break;
        wl_data_offer_receive(selection_, mime.c_str(), fds[1]);
        close(fds[1]);
        saved_.push_back({mime, "", fds[0]});
    }
    wl_display_flush(display_);
    save_state_ = saved_.empty() ? SaveState::None : SaveState::Reading;
}

void WaylandInput::continue_save() {
    if (save_state_ != SaveState::Reading) return;
    size_t total = 0;
    for (const SavedMime& m : saved_) total += m.data.size();
    bool reading = false;
    char chunk[65536];
    for (SavedMime& m : saved_) {
        while (m.fd >= 0) {
            ssize_t n = read(m.fd, chunk, sizeof(chunk));
            if (n > 0) {
                total += n;
                if (total > kMaxSaved) {
                    poe_log("clipboard: over %zu bytes, not keeping it", kMaxSaved);
                    clear_saved();
                    return;
                }
                m.data.append(chunk, n);
                continue;
            }
            if (n < 0 && (errno == EAGAIN || errno == EINTR)) break;
            close(m.fd);  // EOF or error
            m.fd = -1;
        }
        if (m.fd >= 0) reading = true;
    }
    if (reading) return;
    // A type that gave nothing isn't worth offering.
    std::erase_if(saved_, [](const SavedMime& m) { return m.data.empty(); });
    save_state_ = saved_.empty() ? SaveState::None : SaveState::Ready;
    if (save_state_ == SaveState::Ready) poe_log("clipboard: saved %zu type(s)", saved_.size());
}

void WaylandInput::restore_selection() {
    if (save_state_ == SaveState::None) return;  // nothing was saved
    restore_wanted_ = true;
    restore_deadline_ms_ = now_ms() + kRestoreTimeoutMs;
}

void WaylandInput::forget_saved_selection() {
    restore_wanted_ = false;
    if (!own_active()) clear_saved();
}

void WaylandInput::drop_own_source() {
    if (own_source_) wl_data_source_destroy(own_source_);
    if (own_control_source_) ext_data_control_source_v1_destroy(own_control_source_);
    own_source_ = nullptr;
    own_control_source_ = nullptr;
    expect_own_selection_ = false;
}

void WaylandInput::do_restore() {
    restore_wanted_ = false;
    if (saved_.empty()) return;
    std::vector<SavedMime> keep = std::move(saved_);
    drop_own_source();  // an earlier restore's; its cancel mustn't clear `keep`
    saved_ = std::move(keep);
    // Sources come from our managers, so their events come to our queue.
    if (control_device_) {
        // No input serial needed: KWin refuses a core set_selection without
        // a fresh one, and the keys we just sent may not count.
        own_control_source_ = ext_data_control_manager_v1_create_data_source(control_manager_);
        ext_data_control_source_v1_add_listener(own_control_source_, &control_source_listener, this);
        for (const SavedMime& m : saved_) ext_data_control_source_v1_offer(own_control_source_, m.mime.c_str());
        ext_data_control_device_v1_set_selection(control_device_, own_control_source_);
    } else if (data_device_ && data_manager_) {
        own_source_ = wl_data_device_manager_create_data_source(data_manager_);
        wl_data_source_add_listener(own_source_, &source_listener, this);
        for (const SavedMime& m : saved_) wl_data_source_offer(own_source_, m.mime.c_str());
        wl_data_device_set_selection(data_device_, own_source_, last_serial_);
    } else {
        return;
    }
    expect_own_selection_ = true;
    wl_display_flush(display_);
    poe_log("clipboard: restored (%s)", own_control_source_ ? "data-control" : "data device");
}

void WaylandInput::queue_write(const char* mime, int fd) {
    auto it = std::find_if(saved_.begin(), saved_.end(), [&](const SavedMime& m) { return m.mime == mime; });
    if (it == saved_.end()) {
        close(fd);
        return;
    }
    // Written a bit every frame: a slow reader mustn't stall the game.
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    writes_.push_back({fd, it->data});
    continue_writes();
}

void WaylandInput::on_source_send(wl_data_source* source, const char* mime, int fd) {
    if (source != own_source_) {
        close(fd);
        return;
    }
    queue_write(mime, fd);
}

void WaylandInput::on_control_send(ext_data_control_source_v1* source, const char* mime, int fd) {
    if (source != own_control_source_) {
        close(fd);
        return;
    }
    queue_write(mime, fd);
}

void WaylandInput::continue_writes() {
    for (PendingWrite& w : writes_) {
        while (w.fd >= 0 && w.offset < w.data.size()) {
            ssize_t n = write_no_sigpipe(w.fd, w.data.data() + w.offset, w.data.size() - w.offset);
            if (n > 0) {
                w.offset += n;
            } else if (n < 0 && (errno == EAGAIN || errno == EINTR)) {
                break;
            } else {
                w.offset = w.data.size();  // reader went away
            }
        }
        if (w.fd >= 0 && w.offset >= w.data.size()) {
            close(w.fd);
            w.fd = -1;
        }
    }
    std::erase_if(writes_, [](const PendingWrite& w) { return w.fd < 0; });
}

// Something else was copied: our saved content is no longer needed.
void WaylandInput::on_source_cancelled(wl_data_source* source) {
    if (source != own_source_) return;
    drop_own_source();
    clear_saved();
}

void WaylandInput::on_control_cancelled(ext_data_control_source_v1* source) {
    if (source != own_control_source_) return;
    drop_own_source();
    clear_saved();
}

void WaylandInput::on_control_finished() {
    // The compositor withdrew data-control (e.g. the seat went away).
    if (own_control_source_) {
        drop_own_source();
        clear_saved();
    }
    ext_data_control_device_v1_destroy(control_device_);
    control_device_ = nullptr;
}
