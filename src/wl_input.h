// Keyboard and clipboard from the app's own Wayland connection.
//
// The compositor sends wl_keyboard events and clipboard (selection) offers to
// every object of the focused client, so binding our own wl_seat, wl_keyboard
// and wl_data_device on the game's wl_display (on a private event queue)
// lets us see keys and copied text while the game has focus, without hooking
// Wine or reading /dev/input.
#pragma once

#include <cstdint>
#include <set>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

struct wl_display;
struct wl_event_queue;
struct wl_registry;
struct wl_seat;
struct wl_keyboard;
struct wl_data_device_manager;
struct wl_data_device;
struct wl_data_offer;

enum class Hotkey {
    PriceCheck,  // Ctrl+D: we send Ctrl+C ourselves
    CopySeen,    // Ctrl+C pressed by the user
    Close,       // Esc
};

class WaylandInput {
public:
    explicit WaylandInput(wl_display* display);
    ~WaylandInput();

    // Non-blocking: read and dispatch whatever is pending on our queue, and
    // continue any clipboard transfer in progress.
    void poll();

    std::vector<Hotkey> take_hotkeys() { return std::exchange(hotkeys_, {}); }
    bool focused() const { return focused_; }
    bool ctrl_held() const;
    uint32_t last_key() const { return last_key_; }
    bool last_key_down() const { return last_key_down_; }
    const char* status() const;

    // Bumped every time a clipboard transfer completes.
    uint64_t clipboard_seq() const { return clipboard_seq_; }
    const std::string& clipboard_text() const { return clipboard_text_; }

    // Listener callbacks (called by libwayland on our queue).
    void on_global(wl_registry* reg, uint32_t name, const char* iface, uint32_t version);
    void on_seat_caps(uint32_t caps);
    void on_enter(const uint32_t* keys, size_t count);
    void on_leave();
    void on_key(uint32_t key, uint32_t state);
    void on_data_offer(wl_data_offer* offer);
    void on_offer_mime(wl_data_offer* offer, const char* mime);
    void on_dnd_enter(wl_data_offer* offer);
    void on_dnd_leave();
    void on_selection(wl_data_offer* offer);

private:
    void maybe_create_data_device();
    void destroy_offer(wl_data_offer* offer);
    void start_receive(wl_data_offer* offer);
    void continue_receive();
    void cancel_receive();

    wl_display* display_;
    wl_event_queue* queue_ = nullptr;
    wl_registry* registry_ = nullptr;
    wl_seat* seat_ = nullptr;
    wl_keyboard* keyboard_ = nullptr;
    wl_data_device_manager* data_manager_ = nullptr;
    wl_data_device* data_device_ = nullptr;

    bool focused_ = false;
    std::set<uint32_t> held_;
    uint32_t last_key_ = 0;
    bool last_key_down_ = false;
    std::vector<Hotkey> hotkeys_;

    // Best text mime type each live offer advertised ("" = none yet).
    std::unordered_map<wl_data_offer*, std::string> offers_;
    wl_data_offer* selection_ = nullptr;
    wl_data_offer* dnd_offer_ = nullptr;
    int receive_fd_ = -1;
    std::string receive_buf_;
    std::string clipboard_text_;
    uint64_t clipboard_seq_ = 0;
};
