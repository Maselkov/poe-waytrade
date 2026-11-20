// Display labels drawn exactly on their tooltips: the CPU tracking
// (screen_scan.h) works from copies a frame or two old, so a label would
// trail its tooltip and overshoot when the camera turns. Each frame, before
// the overlay draws, a compute shader (shaders/track.comp) finds every
// labelled tooltip's title plate in the very image being presented, around
// where the CPU expects it, and rml.vert moves the label by the difference.
//
// Needs swapchain images that can be sampled (layer.cpp asks for it) and a
// presenting queue that can compute; otherwise, or with the setting off,
// labels go where the CPU tracking puts them.
#pragma once

#include "layer.h"
#include "screen_scan.h"

#include <array>
#include <memory>
#include <vector>

class RmlRenderer;

// One label to place: where it was laid out for (its plate's top-left, as
// the CPU predicts it now), its box, and the plate to look for.
struct GpuLabel {
    float plate_x = 0, plate_y = 0;
    float seen_x = 0, seen_y = 0;  // its last measured place (no prediction)
    std::array<float, 4> rect{};  // x0, y0, x1, y1 (swapchain pixels)
    std::shared_ptr<const ScreenScan::Plate> plate;
};

class GpuTracker {
public:
    GpuTracker(DeviceData* dev, RmlRenderer* renderer);
    ~GpuTracker();
    bool ok() const { return ok_; }

    // A swapchain's images, by their views (made for the framebuffers).
    bool add_swapchain(const void* key, const std::vector<VkImageView>& views, VkFormat format, VkExtent2D extent);
    void remove_swapchain(const void* key);
    bool has_swapchain(const void* key) const;

    // Records the search for `labels` into `cmd` for image `index`, before
    // the render pass (the image in PRESENT_SRC, left so). Returns the label
    // set to hand RmlRenderer::begin_render, or null (nothing recorded).
    VkDescriptorSet record(const void* key, uint32_t index, VkCommandBuffer cmd, VkImage image,
                           const std::vector<GpuLabel>& labels);

private:
    struct Buffer {
        VkBuffer buffer = VK_NULL_HANDLE;
        VkDeviceMemory memory = VK_NULL_HANDLE;
        void* mapped = nullptr;
    };
    struct Image {
        Buffer slots, templates, results, best;
        VkDescriptorSet compute_set = VK_NULL_HANDLE;
        VkDescriptorSet label_set = VK_NULL_HANDLE;
        VkQueryPool queries = VK_NULL_HANDLE;
        bool timed = false;  // its queries hold a measurement to read
        int count = 0;       // labels it searched for
    };
    struct Swapchain {
        const void* key = nullptr;
        VkFormat format = VK_FORMAT_UNDEFINED;
        VkExtent2D extent{};
        std::vector<Image> images;
    };

    bool create_buffer(VkDeviceSize size, Buffer* out);
    void destroy_buffer(Buffer& b);
    void destroy_swapchain(Swapchain& s);
    void read_time(Image& img);

    DeviceData* dev_;
    RmlRenderer* renderer_;
    bool ok_ = false;
    VkSampler sampler_ = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout_ = VK_NULL_HANDLE;
    VkDescriptorPool pool_ = VK_NULL_HANDLE;
    VkPipelineLayout layout_ = VK_NULL_HANDLE;
    VkPipeline pipeline_ = VK_NULL_HANDLE;
    std::vector<std::unique_ptr<Swapchain>> swapchains_;

    // POE_WAYTRADE_TRACK_LOG: the shader's GPU time.
    bool time_log_ = false;
    float timestamp_ns_ = 0;
    double time_sum_ = 0, time_max_ = 0;
    int time_count_ = 0;
    // How far the labels were moved from where the CPU put them.
    double move_sum_ = 0, move_max_ = 0;
    int moved_ = 0, missed_ = 0;
    uint64_t time_since_ = 0;
};
