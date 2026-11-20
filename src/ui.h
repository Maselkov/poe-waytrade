// The overlay's UI: RmlUi documents (assets/ui) bound to a data model that
// mirrors PriceCheck's state.
#pragma once

#include "wl_input.h"

#include <vulkan/vulkan.h>

#include <array>
#include <memory>
#include <vector>

class PriceCheck;
class RmlRenderer;
class ScreenScan;

class OverlayUi {
public:
    // `renderer` becomes RmlUi's render interface and must outlive this.
    explicit OverlayUi(RmlRenderer* renderer);
    ~OverlayUi();
    bool ok() const;

    // Once per frame, before render(): input, state, layout.
    void update(PriceCheck& price_check, ScreenScan& scan, WaylandInput* input, VkExtent2D extent);
    // Inside the overlay's render pass (after RmlRenderer::begin_render).
    // `gpu_labels`: the display labels are moved by the GPU's tracking
    // (gpu_track.h) this frame.
    void render(bool gpu_labels = false);
    // The display labels' boxes (swapchain pixels: x0, y0, x1, y1), in
    // ScreenScan::labels() order, as last laid out.
    std::vector<std::array<float, 4>> scan_label_rects() const;
    // Where the visible panels are, in swapchain pixels.
    std::vector<InputRect> input_rects() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};
