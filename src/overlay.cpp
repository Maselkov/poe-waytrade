// Draws the ImGui overlay into the app's swapchain images right before present.
#include "layer.h"
#include "price_check.h"
#include "wl_input.h"

#include "imgui.h"
#include "imgui_internal.h"
#include "backends/imgui_impl_vulkan.h"

#include <atomic>
#include <cfloat>
#include <chrono>
#include <cmath>
#include <cstdlib>
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

    ImGuiContext* ctx = nullptr;
    bool imgui_ready = false;
    VkFormat imgui_format = VK_FORMAT_UNDEFINED;
    VkRenderPass imgui_render_pass = VK_NULL_HANDLE;

    std::chrono::steady_clock::time_point last_frame = std::chrono::steady_clock::now();
    bool debug = getenv("POE_OVERLAY_DEBUG") != nullptr;
    // Created with the device so the uinput keyboard has time to show up in
    // the compositor before the first hotkey.
    PriceCheck price_check;
    int test_clicks = 0;
};

namespace {

// ImGui's Vulkan backend keeps its function pointers in globals, so the
// overlay can only be bound to one device at a time.
std::atomic<DeviceData*> g_overlay_device{nullptr};

#define VK_CHECK(expr)                                                  \
    do {                                                                \
        VkResult r_ = (expr);                                           \
        if (r_ != VK_SUCCESS) {                                         \
            poe_log("%s failed: %d (%s:%d)", #expr, r_, __FILE__, __LINE__); \
            return false;                                               \
        }                                                               \
    } while (0)

// Command buffers we allocate bypass the loader trampoline, so the loader's
// dispatch pointer has to be patched in by hand.
VKAPI_ATTR VkResult VKAPI_CALL AllocateCommandBuffersFixup(VkDevice device,
                                                           const VkCommandBufferAllocateInfo* info,
                                                           VkCommandBuffer* out) {
    DeviceData* dev = g_overlay_device.load();
    VkResult res = dev->AllocateCommandBuffers(device, info, out);
    if (res == VK_SUCCESS && dev->set_loader_data) {
        for (uint32_t i = 0; i < info->commandBufferCount; i++)
            dev->set_loader_data(device, out[i]);
    }
    return res;
}

PFN_vkVoidFunction imgui_loader(const char* name, void* user) {
    auto* dev = static_cast<DeviceData*>(user);
    if (!strcmp(name, "vkAllocateCommandBuffers")) return (PFN_vkVoidFunction)&AllocateCommandBuffersFixup;
    if (auto fn = dev->next_gdpa(dev->device, name)) return fn;
    return dev->instance->next_gipa(dev->instance->instance, name);
}

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
        VK_CHECK(AllocateCommandBuffersFixup(dev->device, &aci, &img.cmd));

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

void shutdown_imgui(DeviceData* dev, OverlayDevice& ov) {
    if (!ov.imgui_ready) return;
    ImGui::SetCurrentContext(ov.ctx);
    dev->DeviceWaitIdle(dev->device);
    ImGui_ImplVulkan_Shutdown();
    dev->DestroyRenderPass(dev->device, ov.imgui_render_pass, nullptr);
    ov.imgui_render_pass = VK_NULL_HANDLE;
    ov.imgui_ready = false;
}

bool init_imgui(DeviceData* dev, OverlayDevice& ov, OverlaySwapchain& sc, VkQueue queue, uint32_t family) {
    ImGui::SetCurrentContext(ov.ctx);
    // The pipeline is tied to a render pass format; rebuild if it changed.
    if (ov.imgui_ready && ov.imgui_format == sc.format) return true;
    shutdown_imgui(dev, ov);

    if (!ImGui_ImplVulkan_LoadFunctions(dev->instance->api_version, imgui_loader, dev)) {
        poe_log("ImGui_ImplVulkan_LoadFunctions failed");
        return false;
    }

    ov.imgui_render_pass = create_render_pass(dev, sc.format);
    if (!ov.imgui_render_pass) return false;

    ImGui_ImplVulkan_InitInfo info = {};
    info.ApiVersion = dev->instance->api_version;
    info.Instance = dev->instance->instance;
    info.PhysicalDevice = dev->physical_device;
    info.Device = dev->device;
    info.QueueFamily = family;
    info.Queue = queue;
    info.DescriptorPoolSize = IMGUI_IMPL_VULKAN_MINIMUM_SAMPLED_IMAGE_POOL_SIZE;
    info.MinImageCount = sc.min_images < 2 ? 2 : sc.min_images;
    info.ImageCount = (uint32_t)sc.images.size();
    info.PipelineInfoMain.RenderPass = ov.imgui_render_pass;
    info.PipelineInfoMain.Subpass = 0;
    info.PipelineInfoMain.MSAASamples = VK_SAMPLE_COUNT_1_BIT;
    if (!ImGui_ImplVulkan_Init(&info)) {
        poe_log("ImGui_ImplVulkan_Init failed");
        return false;
    }
    ov.imgui_format = sc.format;
    ov.imgui_ready = true;
    return true;
}

// Screen rects of this frame's visible top-level ImGui windows: where the
// mouse should go to us rather than the game.
std::vector<InputRect> panel_rects() {
    std::vector<InputRect> rects;
    for (ImGuiWindow* w : ImGui::GetCurrentContext()->Windows) {
        if (!w->Active || w->Hidden || (w->Flags & ImGuiWindowFlags_ChildWindow)) continue;
        rects.push_back({(int)w->Pos.x, (int)w->Pos.y, (int)ceilf(w->Size.x), (int)ceilf(w->Size.y)});
    }
    return rects;
}

void build_ui(DeviceData* dev, OverlayDevice& ov, const OverlaySwapchain& sc) {
    ImGuiIO& io = ImGui::GetIO();
    auto now = std::chrono::steady_clock::now();
    float dt = std::chrono::duration<float>(now - ov.last_frame).count();
    ov.last_frame = now;
    io.DeltaTime = dt > 0.0f ? dt : 1.0f / 60.0f;
    io.DisplaySize = ImVec2((float)sc.extent.width, (float)sc.extent.height);

    WaylandInput* input = dev->instance->input.get();
    if (input) {
        input->poll();
        ov.price_check.update(*input, now);
        for (const PointerEvent& ev : input->take_pointer_events()) {
            switch (ev.type) {
            case PointerEvent::Motion: io.AddMousePosEvent(ev.x, ev.y); break;
            case PointerEvent::Leave: io.AddMousePosEvent(-FLT_MAX, -FLT_MAX); break;
            case PointerEvent::Button: io.AddMouseButtonEvent(ev.button, ev.down); break;
            case PointerEvent::Wheel: io.AddMouseWheelEvent(0, ev.wheel); break;
            }
        }
        // The compositor cursor is hidden over our panels; draw ImGui's.
        io.MouseDrawCursor = input->pointer_inside();
    }

    ImGui::NewFrame();
    ov.price_check.draw();
    if (ov.debug) {
        ImGui::SetNextWindowPos(ImVec2(io.DisplaySize.x - 520, 40), ImGuiCond_FirstUseEver);
        ImGui::SetNextWindowBgAlpha(0.85f);
        ImGui::Begin("poe-overlay debug", nullptr,
                     ImGuiWindowFlags_AlwaysAutoResize | ImGuiWindowFlags_NoCollapse |
                         ImGuiWindowFlags_NoSavedSettings | ImGuiWindowFlags_NoNav);
        ImGui::Text("Swapchain: %ux%u  format %d", sc.extent.width, sc.extent.height, sc.format);
        ImGui::Text("Frame: %.2f ms  (%.0f FPS)", 1000.0f * io.DeltaTime, io.Framerate);
        ImGui::Separator();
        if (input) {
            ImGui::Text("Input: Wayland (%s)", input->status());
            ImGui::Text("Keyboard focus: %s", input->focused() ? "yes" : "no");
            ImGui::Text("Last key: %u %s", input->last_key(), input->last_key_down() ? "down" : "up");
            ImGui::Text("Clipboard: #%llu, %zu bytes", (unsigned long long)input->clipboard_seq(),
                        input->clipboard_text().size());
            ImGui::Text("Pointer over overlay: %s  (scale %.3f)", input->pointer_inside() ? "yes" : "no",
                        input->scale());
            if (ImGui::Button("Test click")) ov.test_clicks++;
            ImGui::SameLine();
            ImGui::Text("clicked %d times", ov.test_clicks);
        } else {
            ImGui::TextDisabled("Input: none (not a Wayland surface; X11 not implemented)");
        }
        ImGui::End();
    }
    ImGui::Render();

    if (input) input->set_input_rects(panel_rects(), (int)sc.extent.width, (int)sc.extent.height);
}

bool record_and_submit(DeviceData* dev, OverlaySwapchain& sc, uint32_t index, VkQueue queue,
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
    ImGui_ImplVulkan_RenderDrawData(ImGui::GetDrawData(), img.cmd);
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

void overlay_attach(DeviceData* dev) {
    DeviceData* expected = nullptr;
    if (!g_overlay_device.compare_exchange_strong(expected, dev)) {
        poe_log("another device already has the overlay; skipping");
        return;
    }
    dev->overlay = std::make_unique<OverlayDevice>();
    IMGUI_CHECKVERSION();
    dev->overlay->ctx = ImGui::CreateContext();
    ImGuiIO& io = ImGui::GetIO();
    io.IniFilename = nullptr;
    io.LogFilename = nullptr;
    ImGui::StyleColorsDark();
    ImGui::GetStyle().FontScaleMain = 1.5f;
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
    if (targets.empty() || !init_imgui(dev, ov, *targets[0].first, queue, family))
        return dev->QueuePresentKHR(queue, info);

    // Pass 2: build one UI frame, draw it into each image, chain semaphores.
    build_ui(dev, ov, *targets[0].first);

    std::vector<VkSemaphore> signals;
    bool consumed_app_waits = false;
    for (auto& [sc, index] : targets) {
        const VkSemaphore* waits = consumed_app_waits ? nullptr : info->pWaitSemaphores;
        uint32_t wait_count = consumed_app_waits ? 0 : info->waitSemaphoreCount;
        if (!record_and_submit(dev, *sc, index, queue, waits, wait_count)) continue;
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
        shutdown_imgui(dev, ov);
    }
    ImGui::DestroyContext(ov.ctx);
    dev->overlay.reset();
    g_overlay_device.store(nullptr);
}
