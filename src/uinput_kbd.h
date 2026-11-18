// Minimal virtual keyboard on /dev/uinput, used to send Ctrl+C to the game.
//
// Wayland gives clients no way to inject input into other windows, but a
// uinput device is just another keyboard to the compositor, so its keys go
// to whatever has focus. Needs write access to /dev/uinput (systemd's
// uaccess ACL or the input group).
#pragma once

#include <cstdint>
#include <string>

class UinputKeyboard {
public:
    UinputKeyboard();
    ~UinputKeyboard();

    bool ok() const { return fd_ >= 0; }
    const std::string& error() const { return error_; }

    // Queue key changes; nothing is delivered until sync().
    void key(uint16_t code, bool down);
    void sync();

private:
    void emit(uint16_t type, uint16_t code, int32_t value);

    int fd_ = -1;
    std::string error_;
};
