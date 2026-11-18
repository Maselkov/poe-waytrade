# poe-overlay (prototype)

A Vulkan layer that draws an RmlUi panel inside the game's own frames
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
beasts.

Rare and magic items get a search on the official trade site instead: their
mods become filters (minimum at 85% of your rolls, resistances as totals)
that you can toggle with the mouse, and the cheapest online listings show in
the panel, with a button to open the same search in your browser.

## How it works

- `src/layer.cpp`: loader negotiation and dispatch chaining. Intercepts
  instance/device creation, `vkGetDeviceQueue*`, swapchain create/destroy,
  `vkQueuePresentKHR`, and `vkCreateWaylandSurfaceKHR`/`vkDestroySurfaceKHR`.
- `src/overlay.cpp`: on each present, records a render pass (load op LOAD)
  over the swapchain image, draws the UI, waits on the app's semaphores and
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
- `src/ui.cpp`, `src/rml_renderer.cpp`, `assets/ui/`: the UI. RmlUi
  documents (`.rml`) styled by `theme.rcss`, bound to a data model that
  mirrors the price check's state, drawn by our own small Vulkan renderer
  (which also linearises colours for sRGB swapchains). Fonts and UI files
  are built into the `.so` with `#embed`, since the layer can't see host
  paths from inside Steam's container. `theme.rcss` starts with
  `@define $name value;` lines (RCSS has no variables; the loader
  substitutes them), so the palette, accent included, lives in one place.
- `src/wl_pointer.cpp`: mouse input. Pointer events go to the surface under
  the cursor, so the layer attaches a transparent subsurface of its own to
  the game's surface, sized to the visible panels, with an input region
  covering just them: clicks there reach the UI and never the game. With no
  panel visible it's unmapped.
- `daemon/` (Rust): `poe-overlay-daemon` listens on the abstract Unix socket
  `@poe-overlay` (reachable from inside Steam's runtime container, since
  abstract sockets ignore mount namespaces; other users are rejected via
  `SO_PEERCRED`). It loads poe.ninja's PoE1 data for the current league at
  start and every 15 minutes, so lookups never wait on the network. The
  protocol is described at the top of `daemon/src/main.rs`.
  It's socket-activated: systemd owns `@poe-overlay`, starts the daemon on
  the first connection and the daemon exits after 30 minutes idle. The layer
  pings it when the game starts, so prices are loaded by the first lookup.
- Trade search (`daemon/src/trade.rs`, `daemon/src/cdp.rs`): pathofexile.com
  is behind Cloudflare, which refuses plain HTTP clients. Like Awakened PoE
  Trade (Electron's Chromium), the daemon uses a real browser: a headless
  Chromium/Chrome, as-is with its own user agent, driven over the DevTools
  pipe, making same-origin `fetch()` calls from a page on the site. It starts
  on the first rare price check and closes after 10 minutes unused. Mod
  lines map to trade stat ids by their text with numbers replaced by `#`
  (stat list cached daily in `~/.cache/poe-overlay`). The daemon follows
  GGG's `X-Rate-Limit-*` headers and refuses requests that would hit a limit
  rather than risk an IP timeout.

The `.so` exports only the three loader entry points and links libstdc++
statically, so its RmlUi can't clash with another UI library in the process
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
```

This installs the layer (`/usr/lib/libVkLayer_poe_overlay.so` plus a manifest
in `/usr/share/vulkan/implicit_layer.d/`), `/usr/bin/poe-overlay-daemon` and
its systemd user socket + service, and enables the socket for all users
(`systemctl --global`), so there is nothing to start by hand from the next
login on (`systemctl --user start poe-overlay-daemon.socket` for the current
one). Remove with `pacman -R poe-overlay`. The daemon follows poe.ninja's
current softcore league; set `POE_LEAGUE` in the service
(`systemctl --user edit poe-overlay-daemon.service`) to pin one.

## Use with Path of Exile

The layer is inert unless `ENABLE_POE_OVERLAY=1` is set. Steam launch options:

```
ENABLE_POE_OVERLAY=1 PROTON_ENABLE_WAYLAND=1 POE_OVERLAY_LOG=/tmp/poe-overlay.log %command%
```

Without `PROTON_ENABLE_WAYLAND` the game runs on XWayland: the panel still
draws, but hotkeys and the clipboard are not implemented for X11 yet.

`POE_OVERLAY_DEBUG=1` adds a debug window (focus, last key, clipboard state).

Working on the UI without the game:

```bash
POE_OVERLAY_UI_DIR=$PWD/assets POE_OVERLAY_DEMO_ITEM=item.txt \
  VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_OVERLAY=1 vkcube --wsi wayland
```

`POE_OVERLAY_UI_DIR` loads `ui/*` from disk and reloads them when they
change; `POE_OVERLAY_DEMO_ITEM` opens the panel on start with that item text
(as copied from the game). `POE_OVERLAY_UI_SCALE` scales the whole UI.

## Known limitations / next steps

- Hotkeys are hard-coded (Ctrl+D, Esc) and still reach the game too.
- Mouse input under fractional scaling assumes the app renders at full
  resolution if it runs under Wine, at logical size otherwise;
  `POE_OVERLAY_SCALE` overrides the pixel-per-logical-unit ratio.
- No advanced copy (Ctrl+Alt+C) yet, so mod tiers/ranges aren't available.
- No X11 input path (would be `XQueryKeymap` polling like MangoHud).
- Only one device gets the overlay (RmlUi is a process-wide singleton).
- HDR (10-bit/PQ) swapchains aren't colour-corrected yet; sRGB ones are.
- Scarabs, incubators and influenced/ilvl base types aren't priced yet.
- Trade search: no ilvl, influence, socket or corruption filters yet, and
  every matched mod starts enabled (untick some if there are no results).

## License note

`third_party/rmlui` is RmlUi 6.3 core (MIT). `assets/fonts` has Inter and
Cinzel, both under the SIL Open Font License (licence files alongside).
