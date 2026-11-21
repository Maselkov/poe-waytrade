#pragma once

#include <vulkan/vulkan.h>

#include <cstdlib>
#include <cstring>

// HDR swapchains (PoE2's HDR mode; PoE1 has none). The overlay's colours
// are sRGB, so on an HDR swapchain they're mapped to a "paper white": how
// bright the overlay's white is, in nits. That's the compositor's reference
// white for the game's display when it says (wl_color.cpp), so the overlay
// is as bright as other SDR windows; POE_WAYTRADE_HDR_NITS overrides it.
// Without either, ITU-R BT.2408's reference white for graphics in HDR.
constexpr float kDefaultPaperWhite = 203.0f;

// POE_WAYTRADE_HDR_NITS, or 0 if unset or out of range.
inline float hdr_nits_override() {
    static const float nits = [] {
        if (const char* s = getenv("POE_WAYTRADE_HDR_NITS")) {
            float v = strtof(s, nullptr);
            if (v >= 50.0f && v <= 2000.0f) return v;
        }
        return 0.0f;
    }();
    return nits;
}

// The paper white to use, given the compositor's reference white (0 if
// unknown).
inline float hdr_paper_white(float compositor_nits) {
    if (float o = hdr_nits_override()) return o;
    if (compositor_nits >= 50.0f && compositor_nits <= 2000.0f) return compositor_nits;
    return kDefaultPaperWhite;
}

// How rml.frag writes its colour: push constant `mode`.
enum class OutputMode : int {
    Unorm = 0,     // SDR, a UNORM format: sRGB values as they are
    Srgb = 1,      // SDR, an _SRGB format: linear (the format encodes)
    Hdr10 = 2,     // VK_COLOR_SPACE_HDR10_ST2084_EXT: BT.2020, PQ-encoded
    ScrgbLinear = 3,  // VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT: linear BT.709, 1.0 = 80 nits
};

// The colour space the swapchain's pixels are really in. Proton's Wayland
// driver creates HDR swapchains as VK_COLOR_SPACE_PASS_THROUGH_EXT and tells
// the compositor about HDR itself (wp_color_management), so for pass-through
// the format decides: 10-bit is the game's HDR10 (BT.2020, PQ), FP16 is
// scRGB, anything else SDR. POE_WAYTRADE_HDR=hdr10|scrgb|off overrides it.
inline VkColorSpaceKHR effective_color_space(VkFormat format, VkColorSpaceKHR color_space) {
    if (const char* s = getenv("POE_WAYTRADE_HDR")) {
        if (!strcmp(s, "hdr10")) return VK_COLOR_SPACE_HDR10_ST2084_EXT;
        if (!strcmp(s, "scrgb")) return VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT;
        if (!strcmp(s, "off")) return VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
    }
    if (color_space != VK_COLOR_SPACE_PASS_THROUGH_EXT) return color_space;
    switch (format) {
    case VK_FORMAT_A2B10G10R10_UNORM_PACK32:
    case VK_FORMAT_A2R10G10B10_UNORM_PACK32:
        return VK_COLOR_SPACE_HDR10_ST2084_EXT;
    case VK_FORMAT_R16G16B16A16_SFLOAT:
        return VK_COLOR_SPACE_EXTENDED_SRGB_LINEAR_EXT;
    default:
        return VK_COLOR_SPACE_SRGB_NONLINEAR_KHR;
    }
}
