# poe-overlay (prototype)

A Vulkan layer that draws a Dear ImGui panel inside the game's own frames
(MangoHud-style) and reads hotkeys and the clipboard from the game's own
Wayland connection. Works with Path of Exile running under Proton as a
**native Wayland** client.

Hover an item and press **Ctrl+D**: the layer sends Ctrl+C, reads the item
text PoE copies, shows it in a panel, and asks a small daemon for its
poe.ninja price. Your own Ctrl+C on an item does the same. **Esc** closes the
panel. PoE1 only for now.

Priced: currency, fragments, essences, divination cards, omens, oils,
fossils, tattoos and other exchange items; uniques (incl. 5L/6L and
unidentified); skill gems (level/quality/corruption); maps, invitations,
beasts. Rare and magic items need a trade search, which isn't implemented.

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
  It does the same with a `wl_data_device` for the clipboard, draining the
  transfer pipe a bit each frame so the present thread never blocks on Wine.
- `src/uinput_kbd.cpp`: a virtual keyboard on `/dev/uinput` that sends
  Ctrl+C. Wayland has no other way to inject keys into a window; the
  compositor treats the device as a normal keyboard. Needs write access to
  `/dev/uinput` (systemd's uaccess ACL usually grants it to the logged-in
  user; otherwise the `input` group).
- `src/price_check.cpp`: hotkey → Ctrl+C → wait up to 1 s for a new clipboard
  transfer → parse (`src/item_text.cpp`) → show. Only clipboard changes right
  after a Ctrl+C/Ctrl+D count, so stale clipboard contents never pop up.
  The item text then goes to the daemon via `src/daemon_client.cpp`, which
  also never blocks the frame.
- `daemon/` (Rust): `poe-overlay-daemon` listens on the abstract Unix socket
  `@poe-overlay` (reachable from inside Steam's runtime container, since
  abstract sockets ignore mount namespaces; other users are rejected via
  `SO_PEERCRED`). It loads poe.ninja's PoE1 data for the current league at
  start and every 15 minutes, so lookups never wait on the network. The
  protocol is described at the top of `daemon/src/main.rs`.

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

The daemon:

```bash
cd daemon && cargo run --release                 # serve
cargo run --release -- query item.txt            # ask a running daemon
```

## Install (Arch)

```bash
cd packaging/arch && makepkg -si
systemctl --user enable --now poe-overlay-daemon
```

This installs the layer (`/usr/lib/libVkLayer_poe_overlay.so` plus a manifest
in `/usr/share/vulkan/implicit_layer.d/`), `/usr/bin/poe-overlay-daemon` and
its systemd user unit; remove with `pacman -R poe-overlay`. The daemon
follows poe.ninja's current softcore league; set `POE_LEAGUE` in the unit
(`systemctl --user edit poe-overlay-daemon`) to pin one.

## Use with Path of Exile

The layer is inert unless `ENABLE_POE_OVERLAY=1` is set. Steam launch options:

```
ENABLE_POE_OVERLAY=1 PROTON_ENABLE_WAYLAND=1 POE_OVERLAY_LOG=/tmp/poe-overlay.log %command%
```

Without `PROTON_ENABLE_WAYLAND` the game runs on XWayland: the panel still
draws, but hotkeys and the clipboard are not implemented for X11 yet.

`POE_OVERLAY_DEBUG=1` adds a debug window (focus, last key, clipboard state).

## Known limitations / next steps

- Hotkeys are hard-coded (Ctrl+D, Esc) and still reach the game too.
- No mouse input; the panel is display-only.
- No advanced copy (Ctrl+Alt+C) yet, so mod tiers/ranges aren't available.
- No X11 input path (would be `XQueryKeymap` polling like MangoHud).
- Only one device gets the overlay (ImGui's Vulkan backend is global).
- sRGB swapchains render ImGui colours slightly off (no gamma handling).
- Scarabs, incubators and influenced/ilvl base types aren't priced yet.
- Next: trade API search for rares (mapping mod text to trade stat IDs).

## License note

`third_party/imgui` is Dear ImGui v1.93.0 (MIT).
