// The overlay's UI: RmlUi documents (assets/ui) bound to a data model that
// mirrors PriceCheck's state.
#pragma once

#include "wl_input.h"

#include <vulkan/vulkan.h>

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
    void render();
    // Where the visible panels are, in swapchain pixels.
    std::vector<InputRect> input_rects() const;

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};
