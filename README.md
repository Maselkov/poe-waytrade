# poe-overlay (prototype)

A Vulkan layer that draws a Dear ImGui panel inside the game's own frames
(MangoHud-style) and reads hotkeys from the game's own Wayland connection.
Goal of this prototype: prove rendering + hotkeys work with Path of Exile
running under Proton as a **native Wayland** client.

## How it works

- `src/layer.cpp`: loader negotiation and dispatch chaining. Intercepts
  instance/device creation, `vkGetDeviceQueue*`, swapchain create/destroy,
  `vkQueuePresentKHR`, and `vkCreateWaylandSurfaceKHR`/`vkDestroySurfaceKHR`.
- `src/overlay.cpp`: on each present, records a render pass (load op LOAD)
  over the swapchain image, draws ImGui, waits on the app's semaphores and
  hands present our own semaphore instead.
- `src/wl_input.cpp`: takes the `wl_display` the app passed to
  `vkCreateWaylandSurfaceKHR`, binds its own `wl_seat`/`wl_keyboard` on a
  private event queue, and polls it non-blockingly each frame. The compositor
  delivers key events to every keyboard object of the focused client, so this
  sees keys whenever the game has focus, with no Wine hooks or `/dev/input`.

The `.so` exports only the three loader entry points and links libstdc++
statically, so its ImGui can't clash with another one in the process
(e.g. MangoHud's).

## Build and test

```bash
cmake -S . -B build -G Ninja && cmake --build build
```

The build also writes a manifest pointing at the built library, so you can
test without installing:

```bash
VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_OVERLAY=1 vkcube --wsi wayland
```

## Install (Arch)

```bash
cd packaging/arch && makepkg -si
```

This installs `/usr/lib/libVkLayer_poe_overlay.so` and an implicit layer
manifest in `/usr/share/vulkan/implicit_layer.d/`; remove with
`pacman -R poe-overlay`. Other distros: `cmake --install build`.

## Use with Path of Exile

The layer is inert unless `ENABLE_POE_OVERLAY=1` is set. Steam launch options:

```
ENABLE_POE_OVERLAY=1 PROTON_ENABLE_WAYLAND=1 POE_OVERLAY_LOG=/tmp/poe-overlay.log %command%
```

Without `PROTON_ENABLE_WAYLAND` the game runs on XWayland: the panel still
draws, but hotkeys are not implemented for X11 yet.

## Known limitations / next steps

- Hotkey is hard-coded to Ctrl+D and the key also still reaches the game.
- No mouse input; the panel is display-only.
- No X11 input path (would be `XQueryKeymap` polling like MangoHud).
- Only one device gets the overlay (ImGui's Vulkan backend is global).
- sRGB swapchains render ImGui colours slightly off (no gamma handling).
- Not yet tested inside Steam's runtime container (pressure-vessel).
- Next: Ctrl+C injection + clipboard read, then a daemon socket for prices.

## License note

`third_party/imgui` is Dear ImGui v1.93.0 (MIT).
