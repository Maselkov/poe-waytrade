// Keyboard input from the app's own Wayland connection.
//
// The compositor sends wl_keyboard events to every keyboard object of the
// focused client, so binding our own wl_seat/wl_keyboard on the game's
// wl_display (on a private event queue) lets us see keys while the game has
// focus, without hooking Wine or reading /dev/input.
#pragma once

#include <cstdint>
#include <set>

struct wl_display;
struct wl_event_queue;
struct wl_registry;
struct wl_seat;
struct wl_keyboard;

class WaylandInput {
public:
    explicit WaylandInput(wl_display* display);
    ~WaylandInput();

    // Non-blocking: read and dispatch whatever is pending on our queue.
    void poll();

    int take_hotkey_presses() {
        int n = hotkey_presses_;
        hotkey_presses_ = 0;
        return n;
    }
    bool focused() const { return focused_; }
    uint32_t last_key() const { return last_key_; }
    bool last_key_down() const { return last_key_down_; }
    const char* status() const;

    // Listener callbacks (called by libwayland on our queue).
    void on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version);
    void on_seat_caps(uint32_t caps);
    void on_enter(const uint32_t* keys, size_t count);
    void on_leave();
    void on_key(uint32_t key, uint32_t state);

private:
    bool ctrl_held() const;

    wl_display* display_;
    wl_event_queue* queue_ = nullptr;
    wl_registry* registry_ = nullptr;
    wl_seat* seat_ = nullptr;
    wl_keyboard* keyboard_ = nullptr;

    bool focused_ = false;
    std::set<uint32_t> held_;
    uint32_t last_key_ = 0;
    bool last_key_down_ = false;
    int hotkey_presses_ = 0;
};
