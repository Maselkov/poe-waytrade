// Keyboard, clipboard and mouse from the app's own Wayland connection.
//
// The compositor sends wl_keyboard events and clipboard (selection) offers to
// every object of the focused client, so binding our own wl_seat, wl_keyboard
// and wl_data_device on the game's wl_display (on a private event queue)
// lets us see keys and copied text while the game has focus, without hooking
// Wine or reading /dev/input.
//
// Mouse input works differently (wl_pointer.cpp): pointer events go to the
// surface under the cursor, so we put a transparent subsurface of our own
// over the game's surface, with its input region covering just the overlay
// panels. Clicks there reach us and never the game.
#pragma once

#include "hotkeys.h"

#include <cstdint>
#include <optional>
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
struct wl_data_source;
struct ext_data_control_manager_v1;
struct ext_data_control_device_v1;
struct ext_data_control_source_v1;
struct ext_data_control_offer_v1;
struct wl_surface;
struct wl_subsurface;
struct wl_compositor;
struct wl_subcompositor;
struct wl_shm;
struct wl_shm_pool;
struct wl_buffer;
struct wl_pointer;
struct wp_fractional_scale_manager_v1;
struct wp_fractional_scale_v1;

// The game sees every key too (it keeps keyboard focus), so hotkeys must be
// combinations PoE doesn't use. That's also why there's no Esc to close.
// (Typing into a panel takes the keyboard for a moment instead: wl_grab.h.)
// All but CopySeen are bound in the settings (set_bindings); the defaults
// are in the comments.
enum class Hotkey {
    PriceCheck,        // Ctrl+D: quick look; we send Ctrl+C ourselves
    PriceCheckLocked,  // Ctrl+Alt+D: a panel that stays until dismissed
    MapCheck,          // Alt+D: the old map check key, now like Ctrl+Alt+D
    Settings,          // Ctrl+Alt+S: the settings page
    CopySeen,          // Ctrl+C pressed by the user (fixed)
};

struct HotkeyBinding {
    KeyCombo combo;
    Hotkey hotkey;
};

// Positions are in swapchain pixels.
struct PointerEvent {
    enum Type { Motion, Leave, Button, Wheel } type;
    float x = 0, y = 0;  // Motion
    int button = 0;      // Button: 0 left, 1 right, 2 middle
    bool down = false;   // Button
    float wheel = 0;     // Wheel: positive scrolls up
};

struct InputRect {
    int x, y, w, h;  // swapchain pixels
    bool operator==(const InputRect&) const = default;
};

class WaylandInput {
public:
    // `parent` is the game's surface (from vkCreateWaylandSurfaceKHR); our
    // input subsurface is attached to it.
    WaylandInput(wl_display* display, wl_surface* parent);
    ~WaylandInput();

    // Clicks inside these rects go to us instead of the game; empty = none
    // (the subsurface is unmapped). `width`/`height` is the game surface's
    // size: the subsurface must not stick out of it, or the window grows.
    void set_input_rects(const std::vector<InputRect>& rects, int width, int height);
    std::vector<PointerEvent> take_pointer_events() { return std::exchange(pointer_events_, {}); }
    bool pointer_inside() const { return pointer_inside_; }
    // Cursor shown over the overlay panels: a CSS cursor name from RmlUi
    // ("" = default), looked up in the user's cursor theme.
    void set_cursor(const std::string& name);
    // Swapchain pixels per Wayland logical unit.
    double scale() const { return scale_; }
    // The pointer over the game itself (not our panels): its position in
    // swapchain pixels, if known, and clicks the game received.
    bool game_pointer(float* x, float* y) const;
    int take_game_clicks() { return std::exchange(game_clicks_, 0); }

    // Non-blocking: read and dispatch whatever is pending on our queue, and
    // continue any clipboard transfer in progress.
    void poll();

    std::vector<Hotkey> take_hotkeys() { return std::exchange(hotkeys_, {}); }
    // The combos that fire hotkeys. A combo matches its exact modifiers, or
    // with Shift or Super held as well if no binding has those.
    void set_bindings(std::vector<HotkeyBinding> bindings) { bindings_ = std::move(bindings); }
    // Recording a new hotkey: the next key pressed (with the modifiers held)
    // is captured instead of firing anything. The game gets it too.
    void begin_capture() { capturing_ = true, captured_.reset(); }
    void cancel_capture() { capturing_ = false; }
    bool capturing() const { return capturing_; }
    std::optional<KeyCombo> take_captured() { return std::exchange(captured_, std::nullopt); }
    bool focused() const { return focused_; }
    bool ctrl_held() const;
    bool alt_held() const;
    uint32_t mods() const;  // KeyMod bits held
    // All of `mods` held (false for none).
    bool mods_held(uint32_t mods) const { return mods && (this->mods() & mods) == mods; }
    uint32_t last_key() const { return last_key_; }
    bool last_key_down() const { return last_key_down_; }
    const char* status() const;

    // Bumped every time a clipboard transfer completes.
    uint64_t clipboard_seq() const { return clipboard_seq_; }
    const std::string& clipboard_text() const { return clipboard_text_; }

    // Keeping the user's clipboard across our own Ctrl+C (Ctrl+D): save what
    // it holds (every format it's offered in), let the game copy the item,
    // then offer the saved content again as the clipboard.
    void save_selection();
    // The save has finished (or there's nothing to save).
    bool selection_saved() const { return save_state_ != SaveState::Reading; }
    // After the item text arrived: put the saved content back, as soon as
    // the save has finished.
    void restore_selection();
    // The copy didn't happen; the clipboard never changed.
    void forget_saved_selection();

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
    void on_pointer_enter(uint32_t serial, wl_surface* surface, double x, double y);
    void on_pointer_leave(wl_surface* surface);
    void on_pointer_motion(double x, double y);
    void on_pointer_button(uint32_t button, uint32_t state);
    void on_pointer_axis(uint32_t axis, double value);
    void on_preferred_scale(uint32_t scale_120);
    void on_input_serial(uint32_t serial) { last_serial_ = serial; }
    void on_source_send(wl_data_source* source, const char* mime, int fd);
    void on_source_cancelled(wl_data_source* source);
    void on_control_send(ext_data_control_source_v1* source, const char* mime, int fd);
    void on_control_cancelled(ext_data_control_source_v1* source);
    void on_control_finished();

private:
    // wl_pointer.cpp
    void init_scale_policy();
    void bind_pointer_globals(wl_registry* reg, uint32_t name, const char* iface, uint32_t version);
    void update_pointer(bool has_pointer);
    void apply_cursor();
    bool ensure_buffer(int w, int h);
    void destroy_pointer_objects();

    void maybe_create_data_device();
    void destroy_offer(wl_data_offer* offer);
    void start_receive(wl_data_offer* offer);
    void continue_receive();
    void cancel_receive();
    void clear_saved();
    void continue_save();
    void do_restore();
    void continue_writes();
    void queue_write(const char* mime, int fd);
    void drop_own_source();

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
    std::vector<HotkeyBinding> bindings_;
    bool capturing_ = false;
    std::optional<KeyCombo> captured_;

    // Each live offer's mime types, and the best text one ("" = none yet).
    struct OfferInfo {
        std::string text_mime;
        std::vector<std::string> mimes;
    };
    std::unordered_map<wl_data_offer*, OfferInfo> offers_;
    wl_data_offer* selection_ = nullptr;
    wl_data_offer* dnd_offer_ = nullptr;
    int receive_fd_ = -1;
    std::string receive_buf_;
    std::string clipboard_text_;
    uint64_t clipboard_seq_ = 0;

    // The user's clipboard, saved around our own Ctrl+C.
    enum class SaveState { None, Reading, Ready };
    struct SavedMime {
        std::string mime, data;
        int fd = -1;  // still reading
    };
    SaveState save_state_ = SaveState::None;
    std::vector<SavedMime> saved_;
    bool restore_wanted_ = false;
    uint64_t restore_deadline_ms_ = 0;
    // Our source holding the restored content, while it's the clipboard:
    // through ext-data-control if the compositor has it (no input serial
    // needed), else the core data device.
    ext_data_control_manager_v1* control_manager_ = nullptr;
    ext_data_control_device_v1* control_device_ = nullptr;
    ext_data_control_source_v1* own_control_source_ = nullptr;
    wl_data_source* own_source_ = nullptr;
    bool own_active() const { return own_source_ || own_control_source_; }
    bool expect_own_selection_ = false;
    struct PendingWrite {
        int fd;
        std::string data;
        size_t offset = 0;
    };
    std::vector<PendingWrite> writes_;
    uint32_t last_serial_ = 0;  // latest input event, for set_selection

    // Mouse: our input-only subsurface over the game's surface.
    wl_surface* parent_ = nullptr;
    wl_compositor* compositor_ = nullptr;
    wl_subcompositor* subcompositor_ = nullptr;
    wl_shm* shm_ = nullptr;
    wl_pointer* pointer_ = nullptr;
    wl_surface* input_surface_ = nullptr;
    wl_subsurface* input_subsurface_ = nullptr;
    int shm_fd_ = -1;
    size_t shm_size_ = 0;
    wl_shm_pool* shm_pool_ = nullptr;
    wl_buffer* buffer_ = nullptr;
    int buffer_w_ = 0, buffer_h_ = 0;
    bool mapped_ = false;
    int sub_x_ = 0, sub_y_ = 0;  // subsurface position on the game surface (logical)
    // Swapchain pixels vs. logical units: see init_scale_policy().
    double scale_ = 1.0;
    bool follow_fractional_scale_ = false;
    wp_fractional_scale_manager_v1* fractional_manager_ = nullptr;
    wp_fractional_scale_v1* fractional_scale_ = nullptr;
    std::vector<InputRect> rects_;
    bool pointer_inside_ = false;
    int buttons_held_ = 0;  // on our subsurface
    std::vector<PointerEvent> pointer_events_;
    bool pointer_on_game_ = false;
    bool game_pointer_known_ = false;
    float game_x_ = 0, game_y_ = 0;
    int game_clicks_ = 0;
    uint32_t enter_serial_ = 0;
    struct wl_cursor_theme* cursor_theme_ = nullptr;
    wl_surface* cursor_surface_ = nullptr;
    std::string cursor_;          // wanted
    std::string applied_cursor_;  // set since the last pointer enter
};
