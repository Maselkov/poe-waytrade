// Taking the keyboard from the game while the user types into a panel.
//
// The game keeps keyboard focus (the panels are drawn in its frames), so
// normally we only listen in on its keys (wl_input.cpp) and the game acts
// on every one of them. To type a number, we briefly take the keyboard: a
// 1x1 transparent layer-shell surface on the overlay layer with exclusive
// keyboard interactivity. The compositor then sends keys to it alone (the
// game gets a keyboard leave), until we destroy it and focus goes back.
// Clicks pass through: its input region is empty, so the mouse keeps
// working on the panels and the game as before.
//
// It uses a Wayland connection of its own. On the game's connection, Wine's
// keyboard would get the keys too, aimed at a surface it doesn't know.
//
// Needs zwlr_layer_shell_v1 (KDE Plasma, wlroots compositors like Sway and
// Hyprland, COSMIC). GNOME and gamescope don't have it: supported() is
// false there and callers fall back to the wheel and sliders.
#pragma once

#include <cstdint>
#include <utility>
#include <vector>

struct wl_display;
struct wl_registry;
struct wl_compositor;
struct wl_shm;
struct wl_seat;
struct wl_keyboard;
struct wl_surface;
struct wl_buffer;
struct zwlr_layer_shell_v1;
struct zwlr_layer_surface_v1;

class KeyboardGrab {
public:
    KeyboardGrab() = default;
    ~KeyboardGrab();
    KeyboardGrab(const KeyboardGrab&) = delete;
    KeyboardGrab& operator=(const KeyboardGrab&) = delete;

    // Connects on first use (a couple of round trips), then remembers.
    bool supported();
    // Take the keyboard / give it back. take() is a no-op without support.
    void take();
    void release();
    bool active() const { return surface_ != nullptr; }
    // The compositor has given us the keyboard (keys arrive).
    bool focused() const { return focused_; }

    // Non-blocking; once per frame.
    void poll();
    // Key presses (evdev codes) since the last call, with Shift's state.
    struct Key {
        uint32_t code;
        bool shift;
    };
    std::vector<Key> take_keys() { return std::exchange(keys_, {}); }

    // Listener callbacks.
    void on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version);
    void on_seat_caps(uint32_t caps);
    void on_configure(uint32_t serial);
    void on_closed();
    void on_enter();
    void on_leave();
    void on_key(uint32_t key, uint32_t state);

private:
    bool connect();
    void attach_buffer();

    bool tried_ = false;
    wl_display* display_ = nullptr;
    wl_registry* registry_ = nullptr;
    wl_compositor* compositor_ = nullptr;
    wl_shm* shm_ = nullptr;
    wl_seat* seat_ = nullptr;
    wl_keyboard* keyboard_ = nullptr;
    zwlr_layer_shell_v1* layer_shell_ = nullptr;

    wl_surface* surface_ = nullptr;
    zwlr_layer_surface_v1* layer_surface_ = nullptr;
    wl_buffer* buffer_ = nullptr;
    bool focused_ = false;
    bool shift_ = false;
    std::vector<Key> keys_;
};
