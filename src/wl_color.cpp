// The HDR overlay's brightness from the compositor (color-management-v1):
// the reference white of the image description it prefers for the game's
// surface, i.e. how bright SDR white is on the game's display (KDE's "SDR
// brightness", for one). Optional: without the protocol, hdr.h's default.
#include "wl_input.h"
#include "layer.h"

#include <unistd.h>

#include <algorithm>
#include <cstring>

#include <wayland-client.h>

#include "color-management-v1-client-protocol.h"

namespace {

WaylandInput* self(void* data) { return static_cast<WaylandInput*>(data); }

void fb_preferred_changed(void* data, wp_color_management_surface_feedback_v1*, uint32_t) {
    self(data)->request_reference_white();
}
void fb_preferred_changed2(void* data, wp_color_management_surface_feedback_v1*, uint32_t, uint32_t) {
    self(data)->request_reference_white();
}
const wp_color_management_surface_feedback_v1_listener feedback_listener = {
    .preferred_changed = fb_preferred_changed,
    .preferred_changed2 = fb_preferred_changed2,
};

void desc_failed(void* data, wp_image_description_v1*, uint32_t, const char* msg) {
    self(data)->on_description_failed(msg);
}
void desc_ready(void* data, wp_image_description_v1*, uint32_t) { self(data)->on_description_ready(); }
void desc_ready2(void* data, wp_image_description_v1*, uint32_t, uint32_t) { self(data)->on_description_ready(); }
const wp_image_description_v1_listener description_listener = {
    .failed = desc_failed,
    .ready = desc_ready,
    .ready2 = desc_ready2,
};

// Every event needs a handler; only the luminances matter here.
void info_done(void* data, wp_image_description_info_v1*) { self(data)->on_description_info_done(); }
void info_icc(void*, wp_image_description_info_v1*, int32_t fd, uint32_t) { close(fd); }
void info_primaries(void*, wp_image_description_info_v1*, int32_t, int32_t, int32_t, int32_t, int32_t, int32_t,
                    int32_t, int32_t) {}
void info_u32(void*, wp_image_description_info_v1*, uint32_t) {}
void info_luminances(void* data, wp_image_description_info_v1*, uint32_t, uint32_t, uint32_t reference) {
    self(data)->on_description_reference(reference);
}
void info_target_luminance(void*, wp_image_description_info_v1*, uint32_t, uint32_t) {}
const wp_image_description_info_v1_listener info_listener = {
    .done = info_done,
    .icc_file = info_icc,
    .primaries = info_primaries,
    .primaries_named = info_u32,
    .tf_power = info_u32,
    .tf_named = info_u32,
    .luminances = info_luminances,
    .target_primaries = info_primaries,
    .target_luminance = info_target_luminance,
    .target_max_cll = info_u32,
    .target_max_fall = info_u32,
};

}  // namespace

bool WaylandInput::bind_color_globals(wl_registry* reg, uint32_t name, const char* iface, uint32_t version) {
    if (color_manager_ || strcmp(iface, wp_color_manager_v1_interface.name)) return false;
    color_manager_ = static_cast<wp_color_manager_v1*>(
        wl_registry_bind(reg, name, &wp_color_manager_v1_interface, std::min(version, 2u)));
    color_feedback_ = wp_color_manager_v1_get_surface_feedback(color_manager_, parent_);
    wp_color_management_surface_feedback_v1_add_listener(color_feedback_, &feedback_listener, this);
    request_reference_white();
    return true;
}

void WaylandInput::request_reference_white() {
    destroy_color_description();
    color_description_ = wp_color_management_surface_feedback_v1_get_preferred(color_feedback_);
    wp_image_description_v1_add_listener(color_description_, &description_listener, this);
}

void WaylandInput::on_description_failed(const char* msg) {
    poe_log("hdr: no preferred image description from the compositor (%s)", msg ? msg : "");
    destroy_color_description();
}

void WaylandInput::on_description_ready() {
    if (color_info_) return;
    pending_reference_ = 0;
    color_info_ = wp_image_description_v1_get_information(color_description_);
    wp_image_description_info_v1_add_listener(color_info_, &info_listener, this);
}

void WaylandInput::on_description_info_done() {
    // `done` destroys the info object on the compositor's side.
    wp_image_description_info_v1_destroy(color_info_);
    color_info_ = nullptr;
    destroy_color_description();
    if (pending_reference_ && pending_reference_ != reference_white_) {
        reference_white_ = pending_reference_;
        poe_log("hdr: compositor's reference white for the game's display: %u nits", reference_white_);
    }
}

void WaylandInput::destroy_color_description() {
    if (color_info_) {
        wp_image_description_info_v1_destroy(color_info_);
        color_info_ = nullptr;
    }
    if (color_description_) {
        wp_image_description_v1_destroy(color_description_);
        color_description_ = nullptr;
    }
}

void WaylandInput::destroy_color_objects() {
    destroy_color_description();
    if (color_feedback_) wp_color_management_surface_feedback_v1_destroy(color_feedback_);
    if (color_manager_) wp_color_manager_v1_destroy(color_manager_);
    color_feedback_ = nullptr;
    color_manager_ = nullptr;
}
