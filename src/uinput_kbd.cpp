#include "uinput_kbd.h"
#include "layer.h"

#include <fcntl.h>
#include <linux/uinput.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

UinputKeyboard::UinputKeyboard() {
    fd_ = open("/dev/uinput", O_WRONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd_ < 0) {
        error_ = std::string("open /dev/uinput: ") + strerror(errno);
        poe_log("uinput: %s", error_.c_str());
        return;
    }

    uinput_setup setup = {};
    setup.id.bustype = BUS_VIRTUAL;
    setup.id.vendor = 0x1209;  // pid.codes test VID
    setup.id.product = 0x0001;
    strncpy(setup.name, "poe-waytrade virtual keyboard", UINPUT_MAX_NAME_SIZE - 1);

    bool ok = ioctl(fd_, UI_SET_EVBIT, EV_KEY) == 0 &&
              ioctl(fd_, UI_SET_KEYBIT, KEY_LEFTCTRL) == 0 &&
              ioctl(fd_, UI_SET_KEYBIT, KEY_LEFTALT) == 0 &&
              ioctl(fd_, UI_SET_KEYBIT, KEY_C) == 0 &&
              ioctl(fd_, UI_DEV_SETUP, &setup) == 0 &&
              ioctl(fd_, UI_DEV_CREATE) == 0;
    if (!ok) {
        error_ = std::string("uinput setup: ") + strerror(errno);
        poe_log("uinput: %s", error_.c_str());
        close(fd_);
        fd_ = -1;
        return;
    }
    // The compositor picks the device up asynchronously (libinput hotplug),
    // so create it well before the first keypress is needed.
    poe_log("uinput: virtual keyboard created");
}

UinputKeyboard::~UinputKeyboard() {
    if (fd_ < 0) return;
    ioctl(fd_, UI_DEV_DESTROY);
    close(fd_);
}

void UinputKeyboard::emit(uint16_t type, uint16_t code, int32_t value) {
    input_event ev = {};
    ev.type = type;
    ev.code = code;
    ev.value = value;
    if (write(fd_, &ev, sizeof(ev)) != (ssize_t)sizeof(ev))
        poe_log("uinput: write failed: %s", strerror(errno));
}

void UinputKeyboard::key(uint16_t code, bool down) {
    if (fd_ >= 0) emit(EV_KEY, code, down ? 1 : 0);
}

void UinputKeyboard::sync() {
    if (fd_ >= 0) emit(EV_SYN, SYN_REPORT, 0);
}
