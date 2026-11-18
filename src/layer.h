// Shared state for the layer: per-instance and per-device dispatch data.
#pragma once

#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>
#include <vulkan/vk_layer.h>

#include <memory>
#include <mutex>
#include <unordered_map>
#include <vector>

void poe_log(const char* fmt, ...) __attribute__((format(printf, 1, 2)));

class WaylandInput;
struct OverlayDevice;

#define POE_INSTANCE_FUNCS(X) \
    X(DestroyInstance)                        \
    X(GetPhysicalDeviceQueueFamilyProperties) \
    X(CreateWaylandSurfaceKHR)                \
    X(DestroySurfaceKHR)

#define POE_DEVICE_FUNCS(X)                                                        \
    X(DestroyDevice) X(GetDeviceQueue) X(GetDeviceQueue2) X(DeviceWaitIdle)        \
    X(CreateSwapchainKHR) X(DestroySwapchainKHR) X(GetSwapchainImagesKHR)          \
    X(QueuePresentKHR) X(QueueSubmit)                                              \
    X(CreateRenderPass) X(DestroyRenderPass)                                       \
    X(CreateImageView) X(DestroyImageView)                                         \
    X(CreateFramebuffer) X(DestroyFramebuffer)                                     \
    X(CreateCommandPool) X(DestroyCommandPool)                                     \
    X(AllocateCommandBuffers) X(FreeCommandBuffers)                                \
    X(BeginCommandBuffer) X(EndCommandBuffer) X(ResetCommandBuffer)                \
    X(CmdBeginRenderPass) X(CmdEndRenderPass)                                      \
    X(CreateFence) X(DestroyFence) X(WaitForFences) X(ResetFences)                 \
    X(CreateSemaphore) X(DestroySemaphore)

struct InstanceData {
    VkInstance instance = VK_NULL_HANDLE;
    uint32_t api_version = VK_API_VERSION_1_0;
    PFN_vkGetInstanceProcAddr next_gipa = nullptr;
#define X(n) PFN_vk##n n = nullptr;
    POE_INSTANCE_FUNCS(X)
#undef X
    // Created from the first Wayland surface: the app's own wl_display. Torn
    // down with that surface, since the app may disconnect the display any
    // time after (vkcube does so before vkDestroyInstance).
    std::unique_ptr<WaylandInput> input;
    VkSurfaceKHR input_surface = VK_NULL_HANDLE;

    ~InstanceData();
};

struct DeviceData {
    VkDevice device = VK_NULL_HANDLE;
    VkPhysicalDevice physical_device = VK_NULL_HANDLE;
    InstanceData* instance = nullptr;
    PFN_vkGetDeviceProcAddr next_gdpa = nullptr;
    PFN_vkSetDeviceLoaderData set_loader_data = nullptr;
#define X(n) PFN_vk##n n = nullptr;
    POE_DEVICE_FUNCS(X)
#undef X
    std::vector<VkQueueFamilyProperties> families;
    std::mutex queue_lock;
    std::unordered_map<VkQueue, uint32_t> queue_family;  // queue -> family index

    // Only one device gets an overlay (ImGui's Vulkan backend is global).
    std::unique_ptr<OverlayDevice> overlay;

    DeviceData();
    ~DeviceData();
};

// overlay.cpp
void overlay_attach(DeviceData* dev);
void overlay_swapchain_created(DeviceData* dev, VkSwapchainKHR sc, const VkSwapchainCreateInfoKHR* info);
void overlay_swapchain_destroyed(DeviceData* dev, VkSwapchainKHR sc);
VkResult overlay_present(DeviceData* dev, VkQueue queue, const VkPresentInfoKHR* info);
void overlay_device_destroyed(DeviceData* dev);
