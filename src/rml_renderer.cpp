#include "rml_renderer.h"
#include "icons.h"

#include <RmlUi/Core/Math.h>
#include <RmlUi/Core/Vertex.h>

#define STB_IMAGE_IMPLEMENTATION
#define STBI_ONLY_PNG
#define STBI_NO_STDIO
#include "stb_image.h"

#include <algorithm>
#include <cstring>

namespace {

const uint32_t kVertSpv[] = {
#include "rml.vert.inc"
};
const uint32_t kFragSpv[] = {
#include "rml.frag.inc"
};

struct Push {
    float transform[16];
    float translate[2];
    float to_ndc[2];
    int32_t srgb;
    int32_t labels;
};
// Sets of label tracking data: one per swapchain image, a few swapchains.
constexpr uint32_t kMaxLabelSets = 32;

// Frames to wait before freeing what RmlUi released: the overlay waits on a
// swapchain image's fence before reusing it, and swapchains have <= 4 images.
constexpr uint64_t kReleaseDelay = 5;
constexpr uint32_t kMaxTextures = 512;

const float kIdentity[16] = {1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1};

bool is_srgb(VkFormat f) {
    return f == VK_FORMAT_B8G8R8A8_SRGB || f == VK_FORMAT_R8G8B8A8_SRGB || f == VK_FORMAT_A8B8G8R8_SRGB_PACK32;
}

}  // namespace

RmlRenderer::RmlRenderer(DeviceData* dev, VkQueue queue, uint32_t family)
    : dev_(dev), device_(dev->device), queue_(queue), family_(family) {
    memcpy(transform_, kIdentity, sizeof(transform_));
    dev->instance->GetPhysicalDeviceMemoryProperties(dev->physical_device, &mem_props_);
    ok_ = create_static_objects();
    if (ok_) {
        const Rml::byte white[4] = {255, 255, 255, 255};
        white_ = create_texture(white, 1, 1);
        ok_ = white_ != nullptr;
    }
}

RmlRenderer::~RmlRenderer() {
    dev_->DeviceWaitIdle(device_);
    for (auto& [frame, fn] : deferred_) fn();
    deferred_.clear();
    if (white_) destroy_texture(white_);
    if (pipeline_) dev_->DestroyPipeline(device_, pipeline_, nullptr);
    if (layout_) dev_->DestroyPipelineLayout(device_, layout_, nullptr);
    if (pool_) dev_->DestroyDescriptorPool(device_, pool_, nullptr);
    if (set_layout_) dev_->DestroyDescriptorSetLayout(device_, set_layout_, nullptr);
    if (label_pool_) dev_->DestroyDescriptorPool(device_, label_pool_, nullptr);
    if (label_layout_) dev_->DestroyDescriptorSetLayout(device_, label_layout_, nullptr);
    destroy_buffer(no_labels_);
    if (sampler_) dev_->DestroySampler(device_, sampler_, nullptr);
    if (upload_pool_) dev_->DestroyCommandPool(device_, upload_pool_, nullptr);
}

bool RmlRenderer::create_static_objects() {
    VkSamplerCreateInfo sci = {VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO};
    sci.magFilter = sci.minFilter = VK_FILTER_LINEAR;
    sci.addressModeU = sci.addressModeV = sci.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    if (dev_->CreateSampler(device_, &sci, nullptr, &sampler_) != VK_SUCCESS) return false;

    VkDescriptorSetLayoutBinding binding = {};
    binding.descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
    binding.descriptorCount = 1;
    binding.stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT;
    binding.pImmutableSamplers = &sampler_;
    VkDescriptorSetLayoutCreateInfo lci = {VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO};
    lci.bindingCount = 1;
    lci.pBindings = &binding;
    if (dev_->CreateDescriptorSetLayout(device_, &lci, nullptr, &set_layout_) != VK_SUCCESS) return false;

    VkDescriptorPoolSize size = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, kMaxTextures};
    VkDescriptorPoolCreateInfo pci = {VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO};
    pci.flags = VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT;
    pci.maxSets = kMaxTextures;
    pci.poolSizeCount = 1;
    pci.pPoolSizes = &size;
    if (dev_->CreateDescriptorPool(device_, &pci, nullptr, &pool_) != VK_SUCCESS) return false;

    VkDescriptorSetLayoutBinding label_bindings[2] = {};
    for (uint32_t i = 0; i < 2; i++) {
        label_bindings[i].binding = i;
        label_bindings[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        label_bindings[i].descriptorCount = 1;
        label_bindings[i].stageFlags = VK_SHADER_STAGE_VERTEX_BIT;
    }
    lci.bindingCount = 2;
    lci.pBindings = label_bindings;
    if (dev_->CreateDescriptorSetLayout(device_, &lci, nullptr, &label_layout_) != VK_SUCCESS) return false;
    VkDescriptorPoolSize label_size = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, kMaxLabelSets * 2};
    pci.maxSets = kMaxLabelSets;
    pci.pPoolSizes = &label_size;
    if (dev_->CreateDescriptorPool(device_, &pci, nullptr, &label_pool_) != VK_SUCCESS) return false;
    // The stand-in: a count of 0 (and room for a result nobody reads).
    if (!create_buffer(64, VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, &no_labels_)) return false;
    void* zero = nullptr;
    dev_->MapMemory(device_, no_labels_.memory, 0, VK_WHOLE_SIZE, 0, &zero);
    memset(zero, 0, 64);
    no_labels_set_ = create_label_set(no_labels_.buffer, no_labels_.buffer);
    if (!no_labels_set_) return false;

    VkPushConstantRange range = {VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(Push)};
    VkDescriptorSetLayout layouts[2] = {set_layout_, label_layout_};
    VkPipelineLayoutCreateInfo plci = {VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    plci.setLayoutCount = 2;
    plci.pSetLayouts = layouts;
    plci.pushConstantRangeCount = 1;
    plci.pPushConstantRanges = &range;
    if (dev_->CreatePipelineLayout(device_, &plci, nullptr, &layout_) != VK_SUCCESS) return false;

    VkCommandPoolCreateInfo cpci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    cpci.flags = VK_COMMAND_POOL_CREATE_TRANSIENT_BIT;
    cpci.queueFamilyIndex = family_;
    return dev_->CreateCommandPool(device_, &cpci, nullptr, &upload_pool_) == VK_SUCCESS;
}

bool RmlRenderer::set_target(VkRenderPass render_pass, VkFormat format) {
    srgb_ = is_srgb(format);
    if (pipeline_) {
        dev_->DeviceWaitIdle(device_);
        dev_->DestroyPipeline(device_, pipeline_, nullptr);
        pipeline_ = VK_NULL_HANDLE;
    }

    VkShaderModule modules[2] = {};
    const std::pair<const uint32_t*, size_t> code[2] = {{kVertSpv, sizeof(kVertSpv)}, {kFragSpv, sizeof(kFragSpv)}};
    for (int i = 0; i < 2; i++) {
        VkShaderModuleCreateInfo ci = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO};
        ci.codeSize = code[i].second;
        ci.pCode = code[i].first;
        if (dev_->CreateShaderModule(device_, &ci, nullptr, &modules[i]) != VK_SUCCESS) return false;
    }

    VkPipelineShaderStageCreateInfo stages[2] = {};
    for (int i = 0; i < 2; i++) {
        stages[i].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
        stages[i].stage = i == 0 ? VK_SHADER_STAGE_VERTEX_BIT : VK_SHADER_STAGE_FRAGMENT_BIT;
        stages[i].module = modules[i];
        stages[i].pName = "main";
    }

    VkVertexInputBindingDescription vbind = {0, sizeof(Rml::Vertex), VK_VERTEX_INPUT_RATE_VERTEX};
    VkVertexInputAttributeDescription attrs[3] = {
        {0, 0, VK_FORMAT_R32G32_SFLOAT, offsetof(Rml::Vertex, position)},
        {1, 0, VK_FORMAT_R8G8B8A8_UNORM, offsetof(Rml::Vertex, colour)},
        {2, 0, VK_FORMAT_R32G32_SFLOAT, offsetof(Rml::Vertex, tex_coord)},
    };
    VkPipelineVertexInputStateCreateInfo vin = {VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
    vin.vertexBindingDescriptionCount = 1;
    vin.pVertexBindingDescriptions = &vbind;
    vin.vertexAttributeDescriptionCount = 3;
    vin.pVertexAttributeDescriptions = attrs;

    VkPipelineInputAssemblyStateCreateInfo ia = {VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO};
    ia.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
    VkPipelineViewportStateCreateInfo vp = {VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO};
    vp.viewportCount = 1;
    vp.scissorCount = 1;
    VkPipelineRasterizationStateCreateInfo rs = {VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO};
    rs.polygonMode = VK_POLYGON_MODE_FILL;
    rs.cullMode = VK_CULL_MODE_NONE;
    rs.lineWidth = 1.0f;
    VkPipelineMultisampleStateCreateInfo ms = {VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO};
    ms.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;

    // RmlUi colours are premultiplied.
    VkPipelineColorBlendAttachmentState blend = {};
    blend.blendEnable = VK_TRUE;
    blend.srcColorBlendFactor = VK_BLEND_FACTOR_ONE;
    blend.dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA;
    blend.colorBlendOp = VK_BLEND_OP_ADD;
    blend.srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE;
    blend.dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA;
    blend.alphaBlendOp = VK_BLEND_OP_ADD;
    blend.colorWriteMask = 0xF;
    VkPipelineColorBlendStateCreateInfo cb = {VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO};
    cb.attachmentCount = 1;
    cb.pAttachments = &blend;

    VkDynamicState dyn[2] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
    VkPipelineDynamicStateCreateInfo ds = {VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO};
    ds.dynamicStateCount = 2;
    ds.pDynamicStates = dyn;

    VkGraphicsPipelineCreateInfo gp = {VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO};
    gp.stageCount = 2;
    gp.pStages = stages;
    gp.pVertexInputState = &vin;
    gp.pInputAssemblyState = &ia;
    gp.pViewportState = &vp;
    gp.pRasterizationState = &rs;
    gp.pMultisampleState = &ms;
    gp.pColorBlendState = &cb;
    gp.pDynamicState = &ds;
    gp.layout = layout_;
    gp.renderPass = render_pass;
    VkResult res = dev_->CreateGraphicsPipelines(device_, VK_NULL_HANDLE, 1, &gp, nullptr, &pipeline_);
    for (VkShaderModule m : modules) dev_->DestroyShaderModule(device_, m, nullptr);
    if (res != VK_SUCCESS) {
        poe_log("ui: pipeline creation failed: %d", res);
        return false;
    }
    return true;
}

void RmlRenderer::begin_frame() {
    frame_++;
    while (!deferred_.empty() && deferred_.front().first <= frame_) {
        deferred_.front().second();
        deferred_.pop_front();
    }
}

void RmlRenderer::begin_render(VkCommandBuffer cmd, VkExtent2D extent, VkDescriptorSet labels) {
    cmd_ = cmd;
    extent_ = extent;
    scissor_enabled_ = false;
    label_mode_ = false;
    memcpy(transform_, kIdentity, sizeof(transform_));
    dev_->CmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline_);
    VkDescriptorSet set = labels ? labels : no_labels_set_;
    dev_->CmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, layout_, 1, 1, &set, 0, nullptr);
    VkViewport viewport = {0, 0, (float)extent.width, (float)extent.height, 0, 1};
    dev_->CmdSetViewport(cmd, 0, 1, &viewport);
}

VkDescriptorSet RmlRenderer::create_label_set(VkBuffer slots, VkBuffer results) {
    VkDescriptorSetAllocateInfo ai = {VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
    ai.descriptorPool = label_pool_;
    ai.descriptorSetCount = 1;
    ai.pSetLayouts = &label_layout_;
    VkDescriptorSet set = VK_NULL_HANDLE;
    if (dev_->AllocateDescriptorSets(device_, &ai, &set) != VK_SUCCESS) return VK_NULL_HANDLE;
    VkDescriptorBufferInfo infos[2] = {{slots, 0, VK_WHOLE_SIZE}, {results, 0, VK_WHOLE_SIZE}};
    VkWriteDescriptorSet writes[2] = {};
    for (uint32_t i = 0; i < 2; i++) {
        writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
        writes[i].dstSet = set;
        writes[i].dstBinding = i;
        writes[i].descriptorCount = 1;
        writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        writes[i].pBufferInfo = &infos[i];
    }
    dev_->UpdateDescriptorSets(device_, 2, writes, 0, nullptr);
    return set;
}

void RmlRenderer::free_label_set(VkDescriptorSet set) {
    if (set) dev_->FreeDescriptorSets(device_, label_pool_, 1, &set);
}

void RmlRenderer::defer(std::function<void()> fn) {
    deferred_.emplace_back(frame_ + kReleaseDelay, std::move(fn));
}

uint32_t RmlRenderer::memory_type(uint32_t bits, VkMemoryPropertyFlags props) const {
    for (uint32_t i = 0; i < mem_props_.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (mem_props_.memoryTypes[i].propertyFlags & props) == props) return i;
    return UINT32_MAX;
}

bool RmlRenderer::create_buffer(VkDeviceSize size, VkBufferUsageFlags usage, Buffer* out) {
    VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    bci.size = size;
    bci.usage = usage;
    if (dev_->CreateBuffer(device_, &bci, nullptr, &out->buffer) != VK_SUCCESS) return false;
    VkMemoryRequirements req;
    dev_->GetBufferMemoryRequirements(device_, out->buffer, &req);
    VkMemoryAllocateInfo ai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = req.size;
    ai.memoryTypeIndex =
        memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if (ai.memoryTypeIndex == UINT32_MAX || dev_->AllocateMemory(device_, &ai, nullptr, &out->memory) != VK_SUCCESS ||
        dev_->BindBufferMemory(device_, out->buffer, out->memory, 0) != VK_SUCCESS) {
        destroy_buffer(*out);
        return false;
    }
    return true;
}

void RmlRenderer::destroy_buffer(Buffer& b) {
    if (b.buffer) dev_->DestroyBuffer(device_, b.buffer, nullptr);
    if (b.memory) dev_->FreeMemory(device_, b.memory, nullptr);
    b = {};
}

Rml::CompiledGeometryHandle RmlRenderer::CompileGeometry(Rml::Span<const Rml::Vertex> vertices,
                                                         Rml::Span<const int> indices) {
    auto* g = new Geometry;
    VkDeviceSize vsize = vertices.size() * sizeof(Rml::Vertex);
    VkDeviceSize isize = indices.size() * sizeof(int);
    g->index_offset = (vsize + 3) & ~VkDeviceSize(3);
    g->index_count = (uint32_t)indices.size();
    if (!create_buffer(g->index_offset + isize, VK_BUFFER_USAGE_VERTEX_BUFFER_BIT | VK_BUFFER_USAGE_INDEX_BUFFER_BIT,
                       &g->buffer)) {
        delete g;
        return 0;
    }
    void* mapped = nullptr;
    dev_->MapMemory(device_, g->buffer.memory, 0, VK_WHOLE_SIZE, 0, &mapped);
    memcpy(mapped, vertices.data(), vsize);
    memcpy((char*)mapped + g->index_offset, indices.data(), isize);
    // Left mapped: freeing the memory unmaps it.
    return reinterpret_cast<Rml::CompiledGeometryHandle>(g);
}

void RmlRenderer::RenderGeometry(Rml::CompiledGeometryHandle handle, Rml::Vector2f translation,
                                 Rml::TextureHandle texture) {
    auto* g = reinterpret_cast<Geometry*>(handle);
    auto* t = texture ? reinterpret_cast<Texture*>(texture) : white_;
    if (!g || !cmd_) return;

    VkRect2D scissor = scissor_enabled_ ? scissor_ : VkRect2D{{0, 0}, extent_};
    dev_->CmdSetScissor(cmd_, 0, 1, &scissor);

    Push push;
    memcpy(push.transform, transform_, sizeof(push.transform));
    push.translate[0] = translation.x;
    push.translate[1] = translation.y;
    push.to_ndc[0] = 2.0f / extent_.width;
    push.to_ndc[1] = 2.0f / extent_.height;
    push.srgb = srgb_;
    push.labels = label_mode_;
    dev_->CmdPushConstants(cmd_, layout_, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(push),
                           &push);
    dev_->CmdBindDescriptorSets(cmd_, VK_PIPELINE_BIND_POINT_GRAPHICS, layout_, 0, 1, &t->set, 0, nullptr);
    VkDeviceSize zero = 0;
    dev_->CmdBindVertexBuffers(cmd_, 0, 1, &g->buffer.buffer, &zero);
    dev_->CmdBindIndexBuffer(cmd_, g->buffer.buffer, g->index_offset, VK_INDEX_TYPE_UINT32);
    dev_->CmdDrawIndexed(cmd_, g->index_count, 1, 0, 0, 0);
}

void RmlRenderer::ReleaseGeometry(Rml::CompiledGeometryHandle handle) {
    auto* g = reinterpret_cast<Geometry*>(handle);
    if (!g) return;
    defer([this, g] {
        destroy_buffer(g->buffer);
        delete g;
    });
}

// Images are only currency icons, "icon:<id>" (RmlUi may prefix the
// document's folder), decoded from the PNGs in icons.cpp.
Rml::TextureHandle RmlRenderer::LoadTexture(Rml::Vector2i& dimensions, const Rml::String& source) {
    size_t at = source.find("icon:");
    const std::string* png = at == std::string::npos ? nullptr : icons_get(source.substr(at + 5));
    if (!png) return 0;
    int w = 0, h = 0, channels = 0;
    unsigned char* rgba = stbi_load_from_memory((const unsigned char*)png->data(), (int)png->size(), &w, &h, &channels, 4);
    if (!rgba) {
        poe_log("ui: couldn't decode %s", source.c_str());
        return 0;
    }
    // RmlUi works with premultiplied alpha.
    for (int i = 0; i < w * h; i++) {
        unsigned char* p = rgba + i * 4;
        p[0] = (unsigned char)(p[0] * p[3] / 255);
        p[1] = (unsigned char)(p[1] * p[3] / 255);
        p[2] = (unsigned char)(p[2] * p[3] / 255);
    }
    Texture* t = create_texture(rgba, w, h);
    stbi_image_free(rgba);
    if (t) dimensions = {w, h};
    return reinterpret_cast<Rml::TextureHandle>(t);
}

Rml::TextureHandle RmlRenderer::GenerateTexture(Rml::Span<const Rml::byte> source, Rml::Vector2i dimensions) {
    return reinterpret_cast<Rml::TextureHandle>(create_texture(source.data(), dimensions.x, dimensions.y));
}

void RmlRenderer::ReleaseTexture(Rml::TextureHandle texture) {
    auto* t = reinterpret_cast<Texture*>(texture);
    if (t) defer([this, t] { destroy_texture(t); });
}

RmlRenderer::Texture* RmlRenderer::create_texture(const Rml::byte* rgba, int width, int height) {
    auto* t = new Texture;
    VkDeviceSize size = (VkDeviceSize)width * height * 4;

    VkImageCreateInfo ici = {VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
    ici.imageType = VK_IMAGE_TYPE_2D;
    ici.format = VK_FORMAT_R8G8B8A8_UNORM;
    ici.extent = {(uint32_t)width, (uint32_t)height, 1};
    ici.mipLevels = 1;
    ici.arrayLayers = 1;
    ici.samples = VK_SAMPLE_COUNT_1_BIT;
    ici.tiling = VK_IMAGE_TILING_OPTIMAL;
    ici.usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    ici.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    VkMemoryRequirements req;
    VkMemoryAllocateInfo ai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    Buffer staging;
    VkCommandBuffer cmd = VK_NULL_HANDLE;

    auto fail = [&](const char* what) {
        poe_log("ui: texture %dx%d: %s failed", width, height, what);
        if (cmd) dev_->FreeCommandBuffers(device_, upload_pool_, 1, &cmd);
        destroy_buffer(staging);
        destroy_texture(t);
        return nullptr;
    };

    if (dev_->CreateImage(device_, &ici, nullptr, &t->image) != VK_SUCCESS) return fail("image");
    dev_->GetImageMemoryRequirements(device_, t->image, &req);
    ai.allocationSize = req.size;
    ai.memoryTypeIndex = memory_type(req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    if (ai.memoryTypeIndex == UINT32_MAX || dev_->AllocateMemory(device_, &ai, nullptr, &t->memory) != VK_SUCCESS ||
        dev_->BindImageMemory(device_, t->image, t->memory, 0) != VK_SUCCESS)
        return fail("image memory");

    VkImageViewCreateInfo vci = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
    vci.image = t->image;
    vci.viewType = VK_IMAGE_VIEW_TYPE_2D;
    vci.format = ici.format;
    vci.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    if (dev_->CreateImageView(device_, &vci, nullptr, &t->view) != VK_SUCCESS) return fail("view");

    VkDescriptorSetAllocateInfo dai = {VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
    dai.descriptorPool = pool_;
    dai.descriptorSetCount = 1;
    dai.pSetLayouts = &set_layout_;
    if (dev_->AllocateDescriptorSets(device_, &dai, &t->set) != VK_SUCCESS) return fail("descriptor set");
    VkDescriptorImageInfo dii = {VK_NULL_HANDLE, t->view, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
    VkWriteDescriptorSet w = {VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET};
    w.dstSet = t->set;
    w.descriptorCount = 1;
    w.descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER;
    w.pImageInfo = &dii;
    dev_->UpdateDescriptorSets(device_, 1, &w, 0, nullptr);

    // Upload through a staging buffer on a one-off command buffer. Textures
    // are rare (font atlases), so waiting for the queue here is fine.
    if (!create_buffer(size, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, &staging)) return fail("staging buffer");
    void* mapped = nullptr;
    dev_->MapMemory(device_, staging.memory, 0, VK_WHOLE_SIZE, 0, &mapped);
    memcpy(mapped, rgba, size);

    VkCommandBufferAllocateInfo cai = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
    cai.commandPool = upload_pool_;
    cai.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    cai.commandBufferCount = 1;
    if (overlay_allocate_command_buffers(dev_, &cai, &cmd) != VK_SUCCESS) return fail("command buffer");
    VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    dev_->BeginCommandBuffer(cmd, &bi);

    VkImageMemoryBarrier barrier = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
    barrier.srcQueueFamilyIndex = barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.image = t->image;
    barrier.subresourceRange = vci.subresourceRange;
    barrier.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    barrier.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barrier.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    dev_->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, nullptr, 0,
                             nullptr, 1, &barrier);
    VkBufferImageCopy copy = {};
    copy.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    copy.imageExtent = ici.extent;
    dev_->CmdCopyBufferToImage(cmd, staging.buffer, t->image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
    barrier.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    barrier.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    barrier.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    dev_->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, nullptr,
                             0, nullptr, 1, &barrier);
    dev_->EndCommandBuffer(cmd);

    VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
    si.commandBufferCount = 1;
    si.pCommandBuffers = &cmd;
    if (dev_->QueueSubmit(queue_, 1, &si, VK_NULL_HANDLE) != VK_SUCCESS) return fail("upload submit");
    dev_->QueueWaitIdle(queue_);
    dev_->FreeCommandBuffers(device_, upload_pool_, 1, &cmd);
    destroy_buffer(staging);
    return t;
}

void RmlRenderer::destroy_texture(Texture* t) {
    if (t->set) dev_->FreeDescriptorSets(device_, pool_, 1, &t->set);
    if (t->view) dev_->DestroyImageView(device_, t->view, nullptr);
    if (t->image) dev_->DestroyImage(device_, t->image, nullptr);
    if (t->memory) dev_->FreeMemory(device_, t->memory, nullptr);
    delete t;
}

void RmlRenderer::EnableScissorRegion(bool enable) { scissor_enabled_ = enable; }

void RmlRenderer::SetScissorRegion(Rml::Rectanglei region) {
    int x0 = std::clamp(region.Left(), 0, (int)extent_.width);
    int y0 = std::clamp(region.Top(), 0, (int)extent_.height);
    int x1 = std::clamp(region.Right(), x0, (int)extent_.width);
    int y1 = std::clamp(region.Bottom(), y0, (int)extent_.height);
    scissor_ = {{x0, y0}, {(uint32_t)(x1 - x0), (uint32_t)(y1 - y0)}};
}

void RmlRenderer::SetTransform(const Rml::Matrix4f* transform) {
    if (transform)
        memcpy(transform_, transform->data(), sizeof(transform_));
    else
        memcpy(transform_, kIdentity, sizeof(transform_));
}
