// Draws the overlay UI into the app's swapchain images right before present.
#include "layer.h"
#include "price_check.h"
#include "rml_renderer.h"
#include "ui.h"
#include "wl_input.h"

#include <atomic>
#include <chrono>
#include <cstring>

struct OverlayImage {
    VkImageView view = VK_NULL_HANDLE;
    VkFramebuffer framebuffer = VK_NULL_HANDLE;
    VkCommandBuffer cmd = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    VkSemaphore done = VK_NULL_HANDLE;
};

struct OverlaySwapchain {
    VkSwapchainKHR handle = VK_NULL_HANDLE;
    VkFormat format = VK_FORMAT_UNDEFINED;
    VkExtent2D extent = {};
    uint32_t min_images = 2;
    // Set up lazily on first present, once we know which queue presents it.
    bool ready = false;
    bool unsupported = false;
    VkRenderPass render_pass = VK_NULL_HANDLE;
    VkCommandPool pool = VK_NULL_HANDLE;
    std::vector<OverlayImage> images;
};

struct OverlayDevice {
    std::mutex lock;
    std::vector<std::unique_ptr<OverlaySwapchain>> swapchains;

    // The UI is created on first present, once a queue and format are known.
    // ui is destroyed before renderer: shutting RmlUi down releases textures.
    std::unique_ptr<RmlRenderer> renderer;
    std::unique_ptr<OverlayUi> ui;
    VkRenderPass ui_render_pass = VK_NULL_HANDLE;
    VkFormat ui_format = VK_FORMAT_UNDEFINED;
    bool ui_failed = false;

    // Created with the device so the uinput keyboard has time to show up in
    // the compositor before the first hotkey.
    PriceCheck price_check;
};

namespace {

// RmlUi is a process-wide singleton, so the overlay is bound to one device.
std::atomic<DeviceData*> g_overlay_device{nullptr};

#define VK_CHECK(expr)                                                  \
    do {                                                                \
        VkResult r_ = (expr);                                           \
        if (r_ != VK_SUCCESS) {                                         \
            poe_log("%s failed: %d (%s:%d)", #expr, r_, __FILE__, __LINE__); \
            return false;                                               \
        }                                                               \
    } while (0)

VkRenderPass create_render_pass(DeviceData* dev, VkFormat format) {
    VkAttachmentDescription att = {};
    att.format = format;
    att.samples = VK_SAMPLE_COUNT_1_BIT;
    att.loadOp = VK_ATTACHMENT_LOAD_OP_LOAD;  // keep the game's frame
    att.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
    att.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
    att.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
    att.initialLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
    att.finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;

    VkAttachmentReference ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription sub = {};
    sub.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
    sub.colorAttachmentCount = 1;
    sub.pColorAttachments = &ref;

    VkSubpassDependency dep = {};
    dep.srcSubpass = VK_SUBPASS_EXTERNAL;
    dep.dstSubpass = 0;
    dep.srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    dep.dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    dep.dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;

    VkRenderPassCreateInfo ci = {VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO};
    ci.attachmentCount = 1;
    ci.pAttachments = &att;
    ci.subpassCount = 1;
    ci.pSubpasses = &sub;
    ci.dependencyCount = 1;
    ci.pDependencies = &dep;

    VkRenderPass rp = VK_NULL_HANDLE;
    if (dev->CreateRenderPass(dev->device, &ci, nullptr, &rp) != VK_SUCCESS) return VK_NULL_HANDLE;
    return rp;
}

void destroy_swapchain_resources(DeviceData* dev, OverlaySwapchain& sc) {
    if (!sc.ready) return;
    std::vector<VkFence> fences;
    for (auto& img : sc.images) fences.push_back(img.fence);
    dev->WaitForFences(dev->device, (uint32_t)fences.size(), fences.data(), VK_TRUE, UINT64_MAX);
    for (auto& img : sc.images) {
        dev->DestroyFramebuffer(dev->device, img.framebuffer, nullptr);
        dev->DestroyImageView(dev->device, img.view, nullptr);
        dev->DestroyFence(dev->device, img.fence, nullptr);
        dev->DestroySemaphore(dev->device, img.done, nullptr);
    }
    dev->DestroyCommandPool(dev->device, sc.pool, nullptr);
    dev->DestroyRenderPass(dev->device, sc.render_pass, nullptr);
    sc.images.clear();
    sc.ready = false;
}

bool setup_swapchain(DeviceData* dev, OverlaySwapchain& sc, uint32_t family) {
    sc.render_pass = create_render_pass(dev, sc.format);
    if (!sc.render_pass) return false;

    VkCommandPoolCreateInfo pci = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    pci.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT;
    pci.queueFamilyIndex = family;
    VK_CHECK(dev->CreateCommandPool(dev->device, &pci, nullptr, &sc.pool));

    uint32_t count = 0;
    VK_CHECK(dev->GetSwapchainImagesKHR(dev->device, sc.handle, &count, nullptr));
    std::vector<VkImage> images(count);
    VK_CHECK(dev->GetSwapchainImagesKHR(dev->device, sc.handle, &count, images.data()));
    sc.images.resize(count);
    sc.ready = true;  // from here on, destroy_swapchain_resources cleans up

    for (uint32_t i = 0; i < count; i++) {
        OverlayImage& img = sc.images[i];

        VkImageViewCreateInfo vci = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
        vci.image = images[i];
        vci.viewType = VK_IMAGE_VIEW_TYPE_2D;
        vci.format = sc.format;
        vci.subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
        VK_CHECK(dev->CreateImageView(dev->device, &vci, nullptr, &img.view));

        VkFramebufferCreateInfo fci = {VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO};
        fci.renderPass = sc.render_pass;
        fci.attachmentCount = 1;
        fci.pAttachments = &img.view;
        fci.width = sc.extent.width;
        fci.height = sc.extent.height;
        fci.layers = 1;
        VK_CHECK(dev->CreateFramebuffer(dev->device, &fci, nullptr, &img.framebuffer));

        VkCommandBufferAllocateInfo aci = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
        aci.commandPool = sc.pool;
        aci.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
        aci.commandBufferCount = 1;
        VK_CHECK(overlay_allocate_command_buffers(dev, &aci, &img.cmd));

        VkFenceCreateInfo fnci = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
        fnci.flags = VK_FENCE_CREATE_SIGNALED_BIT;
        VK_CHECK(dev->CreateFence(dev->device, &fnci, nullptr, &img.fence));

        VkSemaphoreCreateInfo sci = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        VK_CHECK(dev->CreateSemaphore(dev->device, &sci, nullptr, &img.done));
    }
    poe_log("swapchain ready: %ux%u, format %d, %u images", sc.extent.width, sc.extent.height,
            sc.format, count);
    return true;
}

void shutdown_ui(DeviceData* dev, OverlayDevice& ov) {
    ov.ui.reset();
    ov.renderer.reset();  // waits for the device to go idle
    if (ov.ui_render_pass) dev->DestroyRenderPass(dev->device, ov.ui_render_pass, nullptr);
    ov.ui_render_pass = VK_NULL_HANDLE;
    ov.ui_format = VK_FORMAT_UNDEFINED;
}

bool init_ui(DeviceData* dev, OverlayDevice& ov, OverlaySwapchain& sc, VkQueue queue, uint32_t family) {
    if (ov.ui_failed) return false;
    if (!ov.renderer) {
        ov.renderer = std::make_unique<RmlRenderer>(dev, queue, family);
        if (!ov.renderer->ok()) {
            poe_log("ui: renderer setup failed");
            ov.renderer.reset();
            ov.ui_failed = true;
            return false;
        }
    }
    // The pipeline is tied to a render pass format; rebuild if it changed.
    if (ov.ui_format != sc.format) {
        VkRenderPass rp = create_render_pass(dev, sc.format);
        if (!rp || !ov.renderer->set_target(rp, sc.format)) {
            if (rp) dev->DestroyRenderPass(dev->device, rp, nullptr);
            ov.ui_failed = true;
            return false;
        }
        if (ov.ui_render_pass) dev->DestroyRenderPass(dev->device, ov.ui_render_pass, nullptr);
        ov.ui_render_pass = rp;
        ov.ui_format = sc.format;
    }
    if (!ov.ui) {
        ov.ui = std::make_unique<OverlayUi>(ov.renderer.get());
        if (!ov.ui->ok()) {
            poe_log("ui: RmlUi setup failed");
            ov.ui_failed = true;
            return false;
        }
        poe_log("ui: ready");
    }
    return true;
}

void build_ui(DeviceData* dev, OverlayDevice& ov, const OverlaySwapchain& sc) {
    WaylandInput* input = dev->instance->input.get();
    if (input) {
        input->poll();
        ov.price_check.update(*input, std::chrono::steady_clock::now());
    }
    ov.renderer->begin_frame();
    ov.ui->update(ov.price_check, input, sc.extent);
    if (input) input->set_input_rects(ov.ui->input_rects(), (int)sc.extent.width, (int)sc.extent.height);
}

bool record_and_submit(DeviceData* dev, OverlayDevice& ov, OverlaySwapchain& sc, uint32_t index, VkQueue queue,
                       const VkSemaphore* waits, uint32_t wait_count) {
    OverlayImage& img = sc.images[index];
    VK_CHECK(dev->WaitForFences(dev->device, 1, &img.fence, VK_TRUE, UINT64_MAX));
    VK_CHECK(dev->ResetFences(dev->device, 1, &img.fence));
    VK_CHECK(dev->ResetCommandBuffer(img.cmd, 0));

    VkCommandBufferBeginInfo bi = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    bi.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    VK_CHECK(dev->BeginCommandBuffer(img.cmd, &bi));

    VkRenderPassBeginInfo rbi = {VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO};
    rbi.renderPass = sc.render_pass;
    rbi.framebuffer = img.framebuffer;
    rbi.renderArea.extent = sc.extent;
    dev->CmdBeginRenderPass(img.cmd, &rbi, VK_SUBPASS_CONTENTS_INLINE);
    ov.renderer->begin_render(img.cmd, sc.extent);
    ov.ui->render();
    dev->CmdEndRenderPass(img.cmd);
    VK_CHECK(dev->EndCommandBuffer(img.cmd));

    std::vector<VkPipelineStageFlags> stages(wait_count, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT);
    VkSubmitInfo si = {VK_STRUCTURE_TYPE_SUBMIT_INFO};
    si.waitSemaphoreCount = wait_count;
    si.pWaitSemaphores = waits;
    si.pWaitDstStageMask = stages.data();
    si.commandBufferCount = 1;
    si.pCommandBuffers = &img.cmd;
    si.signalSemaphoreCount = 1;
    si.pSignalSemaphores = &img.done;
    VK_CHECK(dev->QueueSubmit(queue, 1, &si, img.fence));
    return true;
}

OverlaySwapchain* find_swapchain(OverlayDevice& ov, VkSwapchainKHR handle) {
    for (auto& sc : ov.swapchains)
        if (sc->handle == handle) return sc.get();
    return nullptr;
}

}  // namespace

DeviceData::DeviceData() = default;
DeviceData::~DeviceData() = default;

VkResult overlay_allocate_command_buffers(DeviceData* dev, const VkCommandBufferAllocateInfo* info,
                                          VkCommandBuffer* out) {
    VkResult res = dev->AllocateCommandBuffers(dev->device, info, out);
    if (res == VK_SUCCESS && dev->set_loader_data) {
        for (uint32_t i = 0; i < info->commandBufferCount; i++) dev->set_loader_data(dev->device, out[i]);
    }
    return res;
}

void overlay_attach(DeviceData* dev) {
    DeviceData* expected = nullptr;
    if (!g_overlay_device.compare_exchange_strong(expected, dev)) {
        poe_log("another device already has the overlay; skipping");
        return;
    }
    dev->overlay = std::make_unique<OverlayDevice>();
    poe_log("overlay attached to device %p", (void*)dev->device);
}

void overlay_swapchain_created(DeviceData* dev, VkSwapchainKHR handle, const VkSwapchainCreateInfoKHR* info) {
    if (!dev->overlay) return;
    std::lock_guard lk(dev->overlay->lock);
    auto sc = std::make_unique<OverlaySwapchain>();
    sc->handle = handle;
    sc->format = info->imageFormat;
    sc->extent = info->imageExtent;
    sc->min_images = info->minImageCount;
    dev->overlay->swapchains.push_back(std::move(sc));
}

void overlay_swapchain_destroyed(DeviceData* dev, VkSwapchainKHR handle) {
    if (!dev->overlay) return;
    OverlayDevice& ov = *dev->overlay;
    std::lock_guard lk(ov.lock);
    for (auto it = ov.swapchains.begin(); it != ov.swapchains.end(); ++it) {
        if ((*it)->handle != handle) continue;
        destroy_swapchain_resources(dev, **it);
        ov.swapchains.erase(it);
        return;
    }
}

VkResult overlay_present(DeviceData* dev, VkQueue queue, const VkPresentInfoKHR* info) {
    if (!dev->overlay) return dev->QueuePresentKHR(queue, info);
    OverlayDevice& ov = *dev->overlay;
    std::lock_guard lk(ov.lock);

    uint32_t family;
    {
        std::lock_guard qlk(dev->queue_lock);
        auto it = dev->queue_family.find(queue);
        if (it == dev->queue_family.end()) return dev->QueuePresentKHR(queue, info);
        family = it->second;
    }
    if (!(dev->families[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))
        return dev->QueuePresentKHR(queue, info);

    // Pass 1: make sure every swapchain in this present is ready to draw into.
    std::vector<std::pair<OverlaySwapchain*, uint32_t>> targets;
    for (uint32_t i = 0; i < info->swapchainCount; i++) {
        OverlaySwapchain* sc = find_swapchain(ov, info->pSwapchains[i]);
        if (!sc || sc->unsupported) continue;
        if (!sc->ready && !setup_swapchain(dev, *sc, family)) {
            destroy_swapchain_resources(dev, *sc);
            sc->unsupported = true;
            continue;
        }
        targets.emplace_back(sc, info->pImageIndices[i]);
    }
    if (targets.empty() || !init_ui(dev, ov, *targets[0].first, queue, family))
        return dev->QueuePresentKHR(queue, info);

    // Pass 2: build one UI frame, draw it into each image, chain semaphores.
    build_ui(dev, ov, *targets[0].first);

    std::vector<VkSemaphore> signals;
    bool consumed_app_waits = false;
    for (auto& [sc, index] : targets) {
        const VkSemaphore* waits = consumed_app_waits ? nullptr : info->pWaitSemaphores;
        uint32_t wait_count = consumed_app_waits ? 0 : info->waitSemaphoreCount;
        if (!record_and_submit(dev, ov, *sc, index, queue, waits, wait_count)) continue;
        consumed_app_waits = true;
        signals.push_back(sc->images[index].done);
    }
    if (signals.empty()) return dev->QueuePresentKHR(queue, info);

    VkPresentInfoKHR patched = *info;
    patched.waitSemaphoreCount = (uint32_t)signals.size();
    patched.pWaitSemaphores = signals.data();
    return dev->QueuePresentKHR(queue, &patched);
}

void overlay_device_destroyed(DeviceData* dev) {
    if (!dev->overlay) return;
    OverlayDevice& ov = *dev->overlay;
    {
        std::lock_guard lk(ov.lock);
        for (auto& sc : ov.swapchains) destroy_swapchain_resources(dev, *sc);
        ov.swapchains.clear();
        shutdown_ui(dev, ov);
    }
    dev->overlay.reset();
    g_overlay_device.store(nullptr);
}
