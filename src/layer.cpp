// Vulkan layer entry points: loader negotiation, dispatch chaining and the
// handful of calls we intercept. Rendering lives in overlay.cpp.
#include "layer.h"
#include "game.h"
#include "version.h"
#include "wl_input.h"

#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#define POE_EXPORT extern "C" __attribute__((visibility("default")))

void poe_log(const char* fmt, ...) {
    static FILE* file = [] {
        const char* path = getenv("POE_WAYTRADE_LOG");
        return path ? fopen(path, "a") : nullptr;
    }();
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    fprintf(stderr, "[poe-waytrade] %s\n", buf);
    if (file) {
        fprintf(file, "[poe-waytrade] %s\n", buf);
        fflush(file);
    }
}

InstanceData::~InstanceData() {
    // Surface never destroyed: the display may already be gone, so leak the
    // Wayland proxies rather than touch it.
    (void)input.release();
}

namespace {

// Dispatchable handles start with the loader's dispatch table pointer; every
// child of an instance/device shares it, so it works as a lookup key.
template <typename T>
void* key(T handle) { return *reinterpret_cast<void**>(handle); }

std::mutex g_lock;
std::unordered_map<void*, InstanceData*> g_instances;
std::unordered_map<void*, DeviceData*> g_devices;

InstanceData* instance_data(void* k) {
    std::lock_guard lk(g_lock);
    auto it = g_instances.find(k);
    return it == g_instances.end() ? nullptr : it->second;
}

DeviceData* device_data(void* k) {
    std::lock_guard lk(g_lock);
    auto it = g_devices.find(k);
    return it == g_devices.end() ? nullptr : it->second;
}

template <typename Info, VkStructureType SType>
Info* find_chain(const void* pnext, VkLayerFunction fn) {
    auto* info = (Info*)pnext;
    while (info && !(info->sType == SType && info->function == fn))
        info = (Info*)info->pNext;
    return info;
}

// ---- instance --------------------------------------------------------------

VKAPI_ATTR VkResult VKAPI_CALL CreateInstance(const VkInstanceCreateInfo* ci,
                                              const VkAllocationCallbacks* alloc,
                                              VkInstance* out) {
    auto* link = find_chain<VkLayerInstanceCreateInfo, VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO>(
        ci->pNext, VK_LAYER_LINK_INFO);
    if (!link) return VK_ERROR_INITIALIZATION_FAILED;

    PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;

    auto create = (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
    VkResult res = create(ci, alloc, out);
    if (res != VK_SUCCESS) return res;

    static bool logged_version = [] {
        poe_log("poe-waytrade %s (%s)", POE_WAYTRADE_VERSION, game_name());
        return true;
    }();
    (void)logged_version;

    auto* data = new InstanceData;
    data->instance = *out;
    data->next_gipa = gipa;
    if (ci->pApplicationInfo && ci->pApplicationInfo->apiVersion)
        data->api_version = ci->pApplicationInfo->apiVersion;
#define X(n) data->n = (PFN_vk##n)gipa(*out, "vk" #n);
    POE_INSTANCE_FUNCS(X)
#undef X

    std::lock_guard lk(g_lock);
    g_instances[key(*out)] = data;
    return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL DestroyInstance(VkInstance instance, const VkAllocationCallbacks* alloc) {
    InstanceData* data;
    {
        std::lock_guard lk(g_lock);
        auto it = g_instances.find(key(instance));
        if (it == g_instances.end()) return;
        data = it->second;
        g_instances.erase(it);
    }
    data->DestroyInstance(instance, alloc);
    delete data;
}

VKAPI_ATTR VkResult VKAPI_CALL CreateWaylandSurfaceKHR(VkInstance instance,
                                                       const VkWaylandSurfaceCreateInfoKHR* ci,
                                                       const VkAllocationCallbacks* alloc,
                                                       VkSurfaceKHR* out) {
    InstanceData* data = instance_data(key(instance));
    VkResult res = data->CreateWaylandSurfaceKHR(instance, ci, alloc, out);
    if (res == VK_SUCCESS && !data->input) {
        // Piggyback on the app's own Wayland connection: binding our own
        // wl_keyboard there gets us key events whenever the game has focus.
        data->input = std::make_unique<WaylandInput>(ci->display, ci->surface);
        data->input_surface = *out;
    }
    return res;
}

VKAPI_ATTR void VKAPI_CALL DestroySurfaceKHR(VkInstance instance, VkSurfaceKHR surface,
                                             const VkAllocationCallbacks* alloc) {
    InstanceData* data = instance_data(key(instance));
    if (surface && surface == data->input_surface) {
        data->input.reset();
        data->input_surface = VK_NULL_HANDLE;
    }
    data->DestroySurfaceKHR(instance, surface, alloc);
}

// ---- device ----------------------------------------------------------------

VKAPI_ATTR VkResult VKAPI_CALL CreateDevice(VkPhysicalDevice phys, const VkDeviceCreateInfo* ci,
                                            const VkAllocationCallbacks* alloc, VkDevice* out) {
    InstanceData* inst = instance_data(key(phys));
    auto* link = find_chain<VkLayerDeviceCreateInfo, VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO>(
        ci->pNext, VK_LAYER_LINK_INFO);
    auto* cb = find_chain<VkLayerDeviceCreateInfo, VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO>(
        ci->pNext, VK_LOADER_DATA_CALLBACK);
    if (!inst || !link) return VK_ERROR_INITIALIZATION_FAILED;

    PFN_vkGetInstanceProcAddr gipa = link->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr gdpa = link->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    link->u.pLayerInfo = link->u.pLayerInfo->pNext;

    auto create = (PFN_vkCreateDevice)gipa(inst->instance, "vkCreateDevice");
    VkResult res = create(phys, ci, alloc, out);
    if (res != VK_SUCCESS) return res;

    auto* data = new DeviceData;
    data->device = *out;
    data->physical_device = phys;
    data->instance = inst;
    data->next_gdpa = gdpa;
    data->set_loader_data = cb ? cb->u.pfnSetDeviceLoaderData : nullptr;
#define X(n) data->n = (PFN_vk##n)gdpa(*out, "vk" #n);
    POE_DEVICE_FUNCS(X)
#undef X

    uint32_t count = 0;
    inst->GetPhysicalDeviceQueueFamilyProperties(phys, &count, nullptr);
    data->families.resize(count);
    inst->GetPhysicalDeviceQueueFamilyProperties(phys, &count, data->families.data());

    {
        std::lock_guard lk(g_lock);
        g_devices[key(*out)] = data;
    }
    overlay_attach(data);
    return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL DestroyDevice(VkDevice device, const VkAllocationCallbacks* alloc) {
    DeviceData* data;
    {
        std::lock_guard lk(g_lock);
        auto it = g_devices.find(key(device));
        if (it == g_devices.end()) return;
        data = it->second;
        g_devices.erase(it);
    }
    overlay_device_destroyed(data);
    data->DestroyDevice(device, alloc);
    delete data;
}

VKAPI_ATTR void VKAPI_CALL GetDeviceQueue(VkDevice device, uint32_t family, uint32_t index, VkQueue* out) {
    DeviceData* data = device_data(key(device));
    data->GetDeviceQueue(device, family, index, out);
    std::lock_guard lk(data->queue_lock);
    data->queue_family[*out] = family;
}

VKAPI_ATTR void VKAPI_CALL GetDeviceQueue2(VkDevice device, const VkDeviceQueueInfo2* info, VkQueue* out) {
    DeviceData* data = device_data(key(device));
    data->GetDeviceQueue2(device, info, out);
    std::lock_guard lk(data->queue_lock);
    data->queue_family[*out] = info->queueFamilyIndex;
}

VKAPI_ATTR VkResult VKAPI_CALL CreateSwapchainKHR(VkDevice device, const VkSwapchainCreateInfoKHR* ci,
                                                  const VkAllocationCallbacks* alloc, VkSwapchainKHR* out) {
    DeviceData* data = device_data(key(device));
    // We render into the swapchain images, so they must be colour attachments.
    VkSwapchainCreateInfoKHR patched = *ci;
    patched.imageUsage |= VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
    // We copy pixels out of them too: around the cursor at a price check, to
    // tell unidentified uniques apart by their art, and POE_WAYTRADE_SNAPSHOT's
    // whole frame (overlay.cpp). Only if the surface allows it.
    VkSurfaceCapabilitiesKHR caps = {};
    if (data->instance->GetPhysicalDeviceSurfaceCapabilitiesKHR &&
        data->instance->GetPhysicalDeviceSurfaceCapabilitiesKHR(data->physical_device, ci->surface, &caps) ==
            VK_SUCCESS &&
        (caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_SRC_BIT))
        patched.imageUsage |= VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    // And sample them: the display labels' tracking finds their tooltips in
    // the frame on the GPU (gpu_track.h).
    if (caps.supportedUsageFlags & VK_IMAGE_USAGE_SAMPLED_BIT) patched.imageUsage |= VK_IMAGE_USAGE_SAMPLED_BIT;
    VkResult res = data->CreateSwapchainKHR(device, &patched, alloc, out);
    if (res == VK_SUCCESS) overlay_swapchain_created(data, *out, &patched);
    return res;
}

VKAPI_ATTR void VKAPI_CALL DestroySwapchainKHR(VkDevice device, VkSwapchainKHR sc,
                                               const VkAllocationCallbacks* alloc) {
    DeviceData* data = device_data(key(device));
    if (sc) overlay_swapchain_destroyed(data, sc);
    data->DestroySwapchainKHR(device, sc, alloc);
}

VKAPI_ATTR VkResult VKAPI_CALL QueuePresentKHR(VkQueue queue, const VkPresentInfoKHR* info) {
    return overlay_present(device_data(key(queue)), queue, info);
}

// ---- proc address lookup ---------------------------------------------------

PFN_vkVoidFunction VKAPI_CALL GetDeviceProcAddr(VkDevice device, const char* name);

PFN_vkVoidFunction device_intercept(const char* name) {
#define I(n) if (!strcmp(name, "vk" #n)) return (PFN_vkVoidFunction)&n;
    I(GetDeviceProcAddr)
    I(DestroyDevice)
    I(GetDeviceQueue)
    I(GetDeviceQueue2)
    I(CreateSwapchainKHR)
    I(DestroySwapchainKHR)
    I(QueuePresentKHR)
#undef I
    return nullptr;
}

PFN_vkVoidFunction VKAPI_CALL GetDeviceProcAddr(VkDevice device, const char* name) {
    if (auto fn = device_intercept(name)) return fn;
    DeviceData* data = device_data(key(device));
    return data ? data->next_gdpa(device, name) : nullptr;
}

PFN_vkVoidFunction VKAPI_CALL GetInstanceProcAddr(VkInstance instance, const char* name) {
#define I(n) if (!strcmp(name, "vk" #n)) return (PFN_vkVoidFunction)&n;
    I(GetInstanceProcAddr)
    I(CreateInstance)
    I(DestroyInstance)
    I(CreateDevice)
#undef I
    if (!instance) return nullptr;
    InstanceData* data = instance_data(key(instance));
    if (!data) return nullptr;
    if (!strcmp(name, "vkCreateWaylandSurfaceKHR"))
        return data->CreateWaylandSurfaceKHR ? (PFN_vkVoidFunction)&CreateWaylandSurfaceKHR : nullptr;
    if (!strcmp(name, "vkDestroySurfaceKHR"))
        return data->DestroySurfaceKHR ? (PFN_vkVoidFunction)&DestroySurfaceKHR : nullptr;
    if (auto fn = device_intercept(name)) return fn;
    return data->next_gipa(instance, name);
}

}  // namespace

POE_EXPORT VKAPI_ATTR VkResult VKAPI_CALL
vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface* iface) {
    if (iface->loaderLayerInterfaceVersion < 2) return VK_ERROR_INITIALIZATION_FAILED;
    iface->loaderLayerInterfaceVersion = 2;
    iface->pfnGetInstanceProcAddr = GetInstanceProcAddr;
    iface->pfnGetDeviceProcAddr = GetDeviceProcAddr;
    iface->pfnGetPhysicalDeviceProcAddr = nullptr;
    return VK_SUCCESS;
}

POE_EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetInstanceProcAddr(VkInstance i, const char* n) {
    return GetInstanceProcAddr(i, n);
}

POE_EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetDeviceProcAddr(VkDevice d, const char* n) {
    return GetDeviceProcAddr(d, n);
}
