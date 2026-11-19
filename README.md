# poe-overlay (prototype)

A Vulkan layer that draws an RmlUi panel inside the game's own frames
(MangoHud-style) and reads hotkeys and the clipboard from the game's own
Wayland connection. Works with Path of Exile running under Proton as a
**native Wayland** client.

Hover an item and press a hotkey; the layer sends Ctrl+C, reads the item text
PoE copies, and shows it in a panel with its price
(poe.ninja, via a small daemon). PoE1 only for now. Modelled on Awakened PoE
Trade:

- **Ctrl+D**: a quick look. It closes as soon as you move the mouse away. To
  use it (toggle trade filters, open the trade site), keep holding Ctrl and
  move onto it; it then stays until the mouse leaves it with Ctrl released
  (holding Ctrl keeps it open wherever the mouse goes).
- **Ctrl+Alt+D**: a panel that stays open until you click its × or click
  anywhere in the game. Only these locked panels have a ×; the quick look
  closes by itself.
- Your own Ctrl+C on an item opens a quick look too.
- Like APT, the panel sits right next to PoE's inventory when you check an
  item there (or next to the stash for items on the left half of the
  screen). PoE's side panels are 986/1600 of the window height wide.

**Alt+D** on a map (or invitation, heist contract/blueprint, expedition
logbook) opens the map check instead: a one-line verdict at the top ("1 deadly mod ·
2 warnings", "No dangerous mods"), the map's
quantity/rarity/pack size, and its mods. Click a mod to cycle it through
deadly → warning → good → unmarked; marks are per profile (I/II/III) and saved
in `~/.config/poe-overlay/map-mods.json`. Reflect, no regen, less recovery,
−max res, cannot leech, player curses and a few others come pre-marked.

Ctrl+Alt+D and Alt+D send PoE's advanced copy (Ctrl+Alt+C, since Alt is still
held); the daemon understands both formats.

The game keeps keyboard focus (the panel is part of its window), so the
overlay never uses keys PoE does; in particular there's no Esc to close.

Priced: currency, fragments, essences, divination cards, omens, oils,
fossils, tattoos and other exchange items; uniques (incl. 5L/6L and
unidentified); skill gems (level/quality/corruption); maps, invitations,
beasts; Wickerman limbs (Viscera, matched by their implicits).

Identified uniques show their poe.ninja price too, but since that ignores
rolls, they also get trade filters like APT's: one for each mod that rolls
(fixed mods are left out), unticked, with a minimum 10% of the mod's range
below your roll (or exactly a perfect roll). The search finds that unique by
name.

Rare and magic items get filters for the official trade site instead, set up
like Awakened PoE Trade's defaults: life, mana, attributes and resistances
from any mod (crafted ones included) become totals, and only total life and
the resistance totals start ticked; other mods are listed, unticked, with a
minimum 10% below your roll. Pick "Any Ring" or the exact base, and whether
corrupted items count (uncorrupted items exclude them by default). Nothing is
searched until you press **Search**, since every search counts against GGG's
rate limit; changing anything afterwards marks the listings as out of date
until you search again.

Every price check uses PoE's advanced copy (Ctrl+Alt+C), which includes each
mod's tier range. A ticked filter gets a two-handle slider over that range,
like APT's: the left handle sets the minimum, the right one a maximum (at the
far right: none). The minimum defaults to your roll minus 10% (the slider
reaches down to it even below the tier), and a roll at the top of its tier defaults to exactly that value
(tagged "Max"). Weapons
also get physical/elemental DPS and attack speed filters from their
properties. Unticked mods fold behind "+ N more mods".

After a search, the panel shows a one-line estimate (the median of the 10
cheapest listings, with the price range and a "Low value" tag under 5c), then
the listings grouped by price with currency icons, how many there are, and
how long ago they were listed, plus a button to open the same search in your
browser.

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
(as copied from the game; maps open the map check). `POE_OVERLAY_UI_SCALE`
scales the whole UI. `POE_OVERLAY_SNAPSHOT=<file.ppm>` saves one of the app's own rendered frames
(after 300 frames) and `POE_OVERLAY_DEMO_SEARCH=1` makes a demo item search by
itself, for checking the UI without screenshots. `POE_OVERLAY_SOCKET=name`
makes the layer (and
`poe-overlay-daemon query`) talk to a daemon on another abstract socket, for
testing a new daemon next to the installed one.

## Known limitations / next steps

- Hotkeys are hard-coded (Ctrl+D, Ctrl+Alt+D) and still reach the game too.
- Mouse input under fractional scaling assumes the app renders at full
  resolution if it runs under Wine, at logical size otherwise;
  `POE_OVERLAY_SCALE` overrides the pixel-per-logical-unit ratio.
- No advanced copy (Ctrl+Alt+C) yet, so mod tiers/ranges aren't available.
- No X11 input path (would be `XQueryKeymap` polling like MangoHud).
- Only one device gets the overlay (RmlUi is a process-wide singleton).
- HDR (10-bit/PQ) swapchains aren't colour-corrected yet; sRGB ones are.
- Scarabs, incubators and influenced/ilvl base types aren't priced yet.
- Trade search: no item level, influence or socket filters yet, and magic
  items can't be searched by exact base (their name hides it among affixes).

## License note

`third_party/rmlui` is RmlUi 6.3 core (MIT); `third_party/stb` has
stb_image (public domain). `assets/fonts` has Inter and
Cinzel, both under the SIL Open Font License (licence files alongside).
