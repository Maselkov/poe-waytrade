#include "gpu_track.h"
#include "rml_renderer.h"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdlib>
#include <cstring>

namespace {

const uint32_t kTrackSpv[] = {
#include "track.comp.inc"
};

// As track.comp declares them.
struct Slot {
    float center[2];
    float laid[2];
    float rect[4];
    int32_t tw2, th2, offset2;
    float norm2;
    int32_t tw4, th4, offset4;
    float norm4;
};
static_assert(sizeof(Slot) == 64);
struct Header {
    int32_t count, srgb, width, height;
    int32_t radius;
    int32_t pad[3];
};
static_assert(sizeof(Header) == 32);
// Stage 1 tries 5x5 positions (track.comp).
constexpr uint32_t kFinePositions = 25;

// Labels tracked at once, and the room for their plates (half-size grey
// floats: a plate is ~200x32 of them at 1440p, ~300x48 at 4K).
constexpr size_t kMaxLabels = 16;
constexpr size_t kTemplateFloats = 16384;

bool is_srgb(VkFormat f) {
    return f == VK_FORMAT_B8G8R8A8_SRGB || f == VK_FORMAT_R8G8B8A8_SRGB || f == VK_FORMAT_A8B8G8R8_SRGB_PACK32;
}

}  // namespace

GpuTracker::GpuTracker(DeviceData* dev, RmlRenderer* renderer) : dev_(dev), renderer_(renderer) {
    time_log_ = getenv("POE_WAYTRADE_TRACK_LOG") != nullptr;
    VkPhysicalDeviceProperties props;
    dev->instance->GetPhysicalDeviceProperties(dev->physical_device, &props);
    timestamp_ns_ = props.limits.timestampPeriod;

    VkDevice d = dev->device;
    VkSamplerCreateInfo sci = {VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO};
    sci.magFilter = sci.minFilter = VK_FILTER_LINEAR;
    sci.addressModeU = sci.addressModeV = sci.addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE;
    if (dev->CreateSampler(d, &sci, nullptr, &sampler_) != VK_SUCCESS) return;

    VkDescriptorSetLayoutBinding bindings[5] = {};
    for (uint32_t i = 0; i < 5; i++) {
        bindings[i].binding = i;
        bindings[i].descriptorType = i == 0 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
        bindings[i].descriptorCount = 1;
        bindings[i].stageFlags = VK_SHADER_STAGE_COMPUTE_BIT;
    }
    bindings[0].pImmutableSamplers = &sampler_;
    VkDescriptorSetLayoutCreateInfo lci = {VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO};
    lci.bindingCount = 5;
    lci.pBindings = bindings;
    if (dev->CreateDescriptorSetLayout(d, &lci, nullptr, &set_layout_) != VK_SUCCESS) return;

    // A set per swapchain image, for a few swapchains.
    VkDescriptorPoolSize sizes[2] = {{VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 32},
                                     {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 128}};
    VkDescriptorPoolCreateInfo pci = {VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO};
    pci.flags = VK_DESCRIPTOR_POOL_CREATE_FREE_DESCRIPTOR_SET_BIT;
    pci.maxSets = 32;
    pci.poolSizeCount = 2;
    pci.pPoolSizes = sizes;
    if (dev->CreateDescriptorPool(d, &pci, nullptr, &pool_) != VK_SUCCESS) return;

    VkPushConstantRange stage = {VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(int32_t)};
    VkPipelineLayoutCreateInfo plci = {VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    plci.setLayoutCount = 1;
    plci.pSetLayouts = &set_layout_;
    plci.pushConstantRangeCount = 1;
    plci.pPushConstantRanges = &stage;
    if (dev->CreatePipelineLayout(d, &plci, nullptr, &layout_) != VK_SUCCESS) return;

    VkShaderModuleCreateInfo mci = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO};
    mci.codeSize = sizeof(kTrackSpv);
    mci.pCode = kTrackSpv;
    VkShaderModule module = VK_NULL_HANDLE;
    if (dev->CreateShaderModule(d, &mci, nullptr, &module) != VK_SUCCESS) return;
    VkComputePipelineCreateInfo cpci = {VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO};
    cpci.stage.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    cpci.stage.stage = VK_SHADER_STAGE_COMPUTE_BIT;
    cpci.stage.module = module;
    cpci.stage.pName = "main";
    cpci.layout = layout_;
    VkResult res = dev->CreateComputePipelines(d, VK_NULL_HANDLE, 1, &cpci, nullptr, &pipeline_);
    dev->DestroyShaderModule(d, module, nullptr);
    if (res != VK_SUCCESS) {
        poe_log("gpu tracking: pipeline creation failed: %d", res);
        return;
    }
    ok_ = true;
}

GpuTracker::~GpuTracker() {
    VkDevice d = dev_->device;
    dev_->DeviceWaitIdle(d);
    for (auto& s : swapchains_) destroy_swapchain(*s);
    if (pipeline_) dev_->DestroyPipeline(d, pipeline_, nullptr);
    if (layout_) dev_->DestroyPipelineLayout(d, layout_, nullptr);
    if (pool_) dev_->DestroyDescriptorPool(d, pool_, nullptr);
    if (set_layout_) dev_->DestroyDescriptorSetLayout(d, set_layout_, nullptr);
    if (sampler_) dev_->DestroySampler(d, sampler_, nullptr);
}

bool GpuTracker::create_buffer(VkDeviceSize size, Buffer* out) {
    VkDevice d = dev_->device;
    VkBufferCreateInfo bci = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    bci.size = size;
    bci.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    if (dev_->CreateBuffer(d, &bci, nullptr, &out->buffer) != VK_SUCCESS) return false;
    VkMemoryRequirements req;
    dev_->GetBufferMemoryRequirements(d, out->buffer, &req);
    VkPhysicalDeviceMemoryProperties props;
    dev_->instance->GetPhysicalDeviceMemoryProperties(dev_->physical_device, &props);
    VkMemoryAllocateInfo ai = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    ai.allocationSize = req.size;
    ai.memoryTypeIndex = UINT32_MAX;
    VkMemoryPropertyFlags want = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    for (uint32_t i = 0; i < props.memoryTypeCount && ai.memoryTypeIndex == UINT32_MAX; i++)
        if ((req.memoryTypeBits & (1u << i)) && (props.memoryTypes[i].propertyFlags & want) == want)
            ai.memoryTypeIndex = i;
    if (ai.memoryTypeIndex == UINT32_MAX || dev_->AllocateMemory(d, &ai, nullptr, &out->memory) != VK_SUCCESS ||
        dev_->BindBufferMemory(d, out->buffer, out->memory, 0) != VK_SUCCESS ||
        dev_->MapMemory(d, out->memory, 0, VK_WHOLE_SIZE, 0, &out->mapped) != VK_SUCCESS) {
        destroy_buffer(*out);
        return false;
    }
    memset(out->mapped, 0, size);
    return true;
}

void GpuTracker::destroy_buffer(Buffer& b) {
    if (b.buffer) dev_->DestroyBuffer(dev_->device, b.buffer, nullptr);
    if (b.memory) dev_->FreeMemory(dev_->device, b.memory, nullptr);
    b = {};
}

void GpuTracker::destroy_swapchain(Swapchain& s) {
    for (Image& img : s.images) {
        if (img.compute_set) dev_->FreeDescriptorSets(dev_->device, pool_, 1, &img.compute_set);
        renderer_->free_label_set(img.label_set);
        if (img.queries) dev_->DestroyQueryPool(dev_->device, img.queries, nullptr);
        destroy_buffer(img.slots);
        destroy_buffer(img.templates);
        destroy_buffer(img.results);
        destroy_buffer(img.best);
    }
    s.images.clear();
}

bool GpuTracker::has_swapchain(const void* key) const {
    return std::any_of(swapchains_.begin(), swapchains_.end(), [&](auto& s) { return s->key == key; });
}

bool GpuTracker::add_swapchain(const void* key, const std::vector<VkImageView>& views, VkFormat format,
                               VkExtent2D extent) {
    if (!ok_) return false;
    remove_swapchain(key);
    auto s = std::make_unique<Swapchain>();
    s->key = key;
    s->format = format;
    s->extent = extent;
    for (VkImageView view : views) {
        Image img;
        bool made = create_buffer(sizeof(Header) + kMaxLabels * sizeof(Slot), &img.slots) &&
                    create_buffer(kMaxLabels * kTemplateFloats * sizeof(float), &img.templates) &&
                    create_buffer(kMaxLabels * 4 * sizeof(float), &img.results) &&
                    create_buffer(kMaxLabels * 2 * sizeof(uint32_t), &img.best);
        if (made) {
            VkDescriptorSetAllocateInfo ai = {VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
            ai.descriptorPool = pool_;
            ai.descriptorSetCount = 1;
            ai.pSetLayouts = &set_layout_;
            made = dev_->AllocateDescriptorSets(dev_->device, &ai, &img.compute_set) == VK_SUCCESS;
        }
        if (made) {
            VkDescriptorImageInfo image_info = {VK_NULL_HANDLE, view, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
            VkDescriptorBufferInfo buffers[4] = {{img.slots.buffer, 0, VK_WHOLE_SIZE},
                                                 {img.templates.buffer, 0, VK_WHOLE_SIZE},
                                                 {img.results.buffer, 0, VK_WHOLE_SIZE},
                                                 {img.best.buffer, 0, VK_WHOLE_SIZE}};
            VkWriteDescriptorSet writes[5] = {};
            for (uint32_t i = 0; i < 5; i++) {
                writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
                writes[i].dstSet = img.compute_set;
                writes[i].dstBinding = i;
                writes[i].descriptorCount = 1;
                writes[i].descriptorType =
                    i == 0 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER : VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
                if (i == 0)
                    writes[i].pImageInfo = &image_info;
                else
                    writes[i].pBufferInfo = &buffers[i - 1];
            }
            dev_->UpdateDescriptorSets(dev_->device, 5, writes, 0, nullptr);
            img.label_set = renderer_->create_label_set(img.slots.buffer, img.results.buffer);
            made = img.label_set != VK_NULL_HANDLE;
        }
        if (made && time_log_) {
            VkQueryPoolCreateInfo qci = {VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO};
            qci.queryType = VK_QUERY_TYPE_TIMESTAMP;
            qci.queryCount = 2;
            if (dev_->CreateQueryPool(dev_->device, &qci, nullptr, &img.queries) != VK_SUCCESS) img.queries = nullptr;
        }
        s->images.push_back(img);
        if (!made) {
            poe_log("gpu tracking: couldn't set up a swapchain image");
            destroy_swapchain(*s);
            return false;
        }
    }
    swapchains_.push_back(std::move(s));
    return true;
}

void GpuTracker::remove_swapchain(const void* key) {
    for (auto it = swapchains_.begin(); it != swapchains_.end(); ++it) {
        if ((*it)->key != key) continue;
        destroy_swapchain(**it);
        swapchains_.erase(it);
        return;
    }
}

void GpuTracker::read_time(Image& img) {
    if (!img.timed) return;
    img.timed = false;
    uint64_t t[2];
    if (dev_->GetQueryPoolResults(dev_->device, img.queries, 0, 2, sizeof(t), t, sizeof(uint64_t),
                                  VK_QUERY_RESULT_64_BIT) != VK_SUCCESS)
        return;
    double ms = (double)(t[1] - t[0]) * timestamp_ns_ / 1e6;
    const float* r = (const float*)img.results.mapped;
    for (int i = 0; i < img.count; i++, r += 4) {
        if (r[3] < 0.5f) {
            missed_++;
            continue;
        }
        double d = std::hypot(r[0], r[1]);
        move_sum_ += d;
        move_max_ = std::max(move_max_, d);
        moved_++;
    }
    time_sum_ += ms;
    time_max_ = std::max(time_max_, ms);
    time_count_++;
    uint64_t now = std::chrono::steady_clock::now().time_since_epoch().count();
    if (now - time_since_ > 2'000'000'000ull) {
        poe_log("gpu tracking: %d frames, %.3f ms average, %.3f ms worst; labels found %d, missed %d, moved %.1f px "
                "average, %.0f px most",
                time_count_, time_sum_ / time_count_, time_max_, moved_, missed_, moved_ ? move_sum_ / moved_ : 0.0,
                move_max_);
        time_since_ = now;
        time_sum_ = time_max_ = 0;
        time_count_ = 0;
        move_sum_ = move_max_ = 0;
        moved_ = missed_ = 0;
    }
}

VkDescriptorSet GpuTracker::record(const void* key, uint32_t index, VkCommandBuffer cmd, VkImage image,
                                   const std::vector<GpuLabel>& labels) {
    Swapchain* s = nullptr;
    for (auto& c : swapchains_)
        if (c->key == key) s = c.get();
    if (!s || index >= s->images.size() || labels.empty()) return VK_NULL_HANDLE;
    Image& img = s->images[index];
    read_time(img);  // its last frame is done: the caller waited on its fence

    // The slots and their plates; the GPU is done with this image's last use.
    auto* header = (Header*)img.slots.mapped;
    auto* slots = (Slot*)((char*)img.slots.mapped + sizeof(Header));
    auto* templ = (float*)img.templates.mapped;
    int count = 0;
    size_t used = 0;
    float spread = 0;  // the furthest a prediction ran from its measurement
    for (const GpuLabel& l : labels) {
        if (count == (int)kMaxLabels) break;
        if (!l.plate) continue;
        const ScreenScan::Template& t2 = l.plate->t2;
        const ScreenScan::Template& t4 = l.plate->t4;
        size_t n2 = (size_t)t2.w * t2.h, n4 = (size_t)t4.w * t4.h;
        if (n2 == 0 || n4 == 0 || t2.v.size() != n2 || t4.v.size() != n4 ||
            used + n2 + n4 > kMaxLabels * kTemplateFloats)
            continue;
        Slot& slot = slots[count++];
        // Search between where it was measured and where it's predicted:
        // after a sudden turn it's somewhere in between, not ahead.
        slot.laid[0] = l.plate_x;
        slot.laid[1] = l.plate_y;
        slot.center[0] = (l.plate_x + l.seen_x) / 2;
        slot.center[1] = (l.plate_y + l.seen_y) / 2;
        spread = std::max({spread, std::abs(l.plate_x - l.seen_x) / 2, std::abs(l.plate_y - l.seen_y) / 2});
        memcpy(slot.rect, l.rect.data(), sizeof(slot.rect));
        memcpy(templ + used, t2.v.data(), n2 * sizeof(float));
        slot.tw2 = t2.w;
        slot.th2 = t2.h;
        slot.offset2 = (int32_t)used;
        slot.norm2 = t2.norm;
        used += n2;
        memcpy(templ + used, t4.v.data(), n4 * sizeof(float));
        slot.tw4 = t4.w;
        slot.th4 = t4.h;
        slot.offset4 = (int32_t)used;
        slot.norm4 = t4.norm;
        used += n4;
    }
    if (count == 0) return VK_NULL_HANDLE;
    img.count = count;
    // A margin (48 pixels at 1440p: the UI scales with the height) beyond
    // the span the prediction covers, up to 96; a multiple of the coarse
    // step. Less misses sharp turns: the prediction overshoots past it.
    int margin = std::max(24, (int)s->extent.height / 30);
    int radius = std::min(96, (margin + (int)std::ceil(spread) + 3) & ~3);
    *header = {count, is_srgb(s->format), (int32_t)s->extent.width, (int32_t)s->extent.height, radius, {}};
    uint32_t coarse = (uint32_t)(radius / 2 + 1) * (uint32_t)(radius / 2 + 1);

    if (img.queries) {
        dev_->CmdResetQueryPool(cmd, img.queries, 0, 2);
        dev_->CmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, img.queries, 0);
    }
    VkImageMemoryBarrier barrier = {VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER};
    barrier.srcQueueFamilyIndex = barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    barrier.image = image;
    barrier.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    barrier.oldLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    barrier.newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barrier.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT | VK_ACCESS_TRANSFER_READ_BIT;
    barrier.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    dev_->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_TRANSFER_BIT,
                             VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, 0, nullptr, 0, nullptr, 1, &barrier);
    dev_->CmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_);
    dev_->CmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, layout_, 0, 1, &img.compute_set, 0, nullptr);
    // Coarse, fine, finish: each reads what the last wrote.
    const uint32_t positions[3] = {coarse, kFinePositions, 1};
    for (int32_t stage = 0; stage < 3; stage++) {
        if (stage > 0) {
            VkMemoryBarrier mb = {VK_STRUCTURE_TYPE_MEMORY_BARRIER};
            mb.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT;
            mb.dstAccessMask = VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT;
            dev_->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                                     0, 1, &mb, 0, nullptr, 0, nullptr);
        }
        dev_->CmdPushConstants(cmd, layout_, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(stage), &stage);
        dev_->CmdDispatch(cmd, (uint32_t)count, positions[stage], 1);
    }

    // Back for drawing; the results to the label shader.
    barrier.oldLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    barrier.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    barrier.srcAccessMask = VK_ACCESS_SHADER_READ_BIT;
    barrier.dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    VkBufferMemoryBarrier results = {VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER};
    results.srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT;
    results.dstAccessMask = VK_ACCESS_SHADER_READ_BIT;
    results.srcQueueFamilyIndex = results.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    results.buffer = img.results.buffer;
    results.size = VK_WHOLE_SIZE;
    dev_->CmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                             VK_PIPELINE_STAGE_VERTEX_SHADER_BIT | VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, 0,
                             0, nullptr, 1, &results, 1, &barrier);
    if (img.queries) {
        dev_->CmdWriteTimestamp(cmd, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, img.queries, 1);
        img.timed = true;
    }
    return img.label_set;
}
