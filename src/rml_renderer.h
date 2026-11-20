// RmlUi render interface on the game's Vulkan device: draws into the render
// pass overlay.cpp has open on the swapchain image.
#pragma once

#include "layer.h"

#include <RmlUi/Core/RenderInterface.h>

#include <deque>
#include <functional>

class RmlRenderer final : public Rml::RenderInterface {
public:
    RmlRenderer(DeviceData* dev, VkQueue queue, uint32_t family);
    ~RmlRenderer() override;
    bool ok() const { return ok_; }

    // (Re)build the pipeline for this render pass/format.
    bool set_target(VkRenderPass render_pass, VkFormat format);
    // Once per frame, after the frame's fence wait: frees resources RmlUi
    // released a few frames ago, which the GPU can no longer be using.
    void begin_frame();
    // Point drawing at a command buffer inside an open render pass.
    // `labels`: the display labels' tracking data for this frame
    // (gpu_track.h), or null for none.
    void begin_render(VkCommandBuffer cmd, VkExtent2D extent, VkDescriptorSet labels = VK_NULL_HANDLE);
    // What's drawn next is the display labels: rml.vert moves each by where
    // the GPU found its tooltip this frame.
    void set_label_mode(bool on) { label_mode_ = on; }

    // The tracking data's descriptor set (rml.vert's set 1: the slots and
    // results buffers), for gpu_track.cpp.
    VkDescriptorSet create_label_set(VkBuffer slots, VkBuffer results);
    void free_label_set(VkDescriptorSet set);

    Rml::CompiledGeometryHandle CompileGeometry(Rml::Span<const Rml::Vertex> vertices,
                                                Rml::Span<const int> indices) override;
    void RenderGeometry(Rml::CompiledGeometryHandle geometry, Rml::Vector2f translation,
                        Rml::TextureHandle texture) override;
    void ReleaseGeometry(Rml::CompiledGeometryHandle geometry) override;
    Rml::TextureHandle LoadTexture(Rml::Vector2i& dimensions, const Rml::String& source) override;
    Rml::TextureHandle GenerateTexture(Rml::Span<const Rml::byte> source, Rml::Vector2i dimensions) override;
    void ReleaseTexture(Rml::TextureHandle texture) override;
    void EnableScissorRegion(bool enable) override;
    void SetScissorRegion(Rml::Rectanglei region) override;
    void SetTransform(const Rml::Matrix4f* transform) override;

private:
    struct Buffer {
        VkBuffer buffer = VK_NULL_HANDLE;
        VkDeviceMemory memory = VK_NULL_HANDLE;
    };
    struct Geometry {
        Buffer buffer;
        VkDeviceSize index_offset = 0;
        uint32_t index_count = 0;
    };
    struct Texture {
        VkImage image = VK_NULL_HANDLE;
        VkDeviceMemory memory = VK_NULL_HANDLE;
        VkImageView view = VK_NULL_HANDLE;
        VkDescriptorSet set = VK_NULL_HANDLE;
    };

    bool create_static_objects();
    bool create_buffer(VkDeviceSize size, VkBufferUsageFlags usage, Buffer* out);
    void destroy_buffer(Buffer& b);
    uint32_t memory_type(uint32_t bits, VkMemoryPropertyFlags props) const;
    Texture* create_texture(const Rml::byte* rgba, int width, int height);
    void destroy_texture(Texture* t);
    void defer(std::function<void()> fn);

    DeviceData* dev_;
    VkDevice device_;
    VkQueue queue_;
    uint32_t family_;
    bool ok_ = false;

    VkPhysicalDeviceMemoryProperties mem_props_ = {};
    VkSampler sampler_ = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout_ = VK_NULL_HANDLE;
    VkDescriptorPool pool_ = VK_NULL_HANDLE;
    // Set 1: the display labels' tracking data, and a stand-in (no labels)
    // for when there's none.
    VkDescriptorSetLayout label_layout_ = VK_NULL_HANDLE;
    VkDescriptorPool label_pool_ = VK_NULL_HANDLE;
    Buffer no_labels_;
    VkDescriptorSet no_labels_set_ = VK_NULL_HANDLE;
    bool label_mode_ = false;
    VkPipelineLayout layout_ = VK_NULL_HANDLE;
    VkPipeline pipeline_ = VK_NULL_HANDLE;
    VkCommandPool upload_pool_ = VK_NULL_HANDLE;
    Texture* white_ = nullptr;  // for untextured geometry
    bool srgb_ = false;

    // Per render
    VkCommandBuffer cmd_ = VK_NULL_HANDLE;
    VkExtent2D extent_ = {};
    bool scissor_enabled_ = false;
    VkRect2D scissor_ = {};
    float transform_[16];

    uint64_t frame_ = 0;
    std::deque<std::pair<uint64_t, std::function<void()>>> deferred_;
};
