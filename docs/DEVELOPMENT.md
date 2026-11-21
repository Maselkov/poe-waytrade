# Developing Waytrade

Waytrade has two parts:

- **The layer** (`src/`, C++): a Vulkan layer loaded into the game. It draws
  the UI into the game's frames and reads input from the game's own Wayland
  connection.
- **The daemon** (`daemon/`, Rust): `poe-waytrade-daemon`. It parses item text,
  holds poe.ninja prices, and runs trade searches.

The layer runs inside Steam's runtime container and the daemon runs on the
host. They talk over an abstract Unix socket.

## How it works

### The layer

- `src/layer.cpp`: loader negotiation and dispatch chaining. It intercepts
  instance and device creation, `vkGetDeviceQueue*`, swapchain
  create/destroy, `vkQueuePresentKHR`, and
  `vkCreateWaylandSurfaceKHR`/`vkDestroySurfaceKHR`.
- `src/overlay.cpp`: on each present, records a render pass (load op LOAD)
  over the swapchain image and draws the UI. It waits on the app's
  semaphores and hands present our own semaphore instead.
- `src/wl_input.cpp`: keys and the clipboard.
  - It takes the `wl_display` the app passed to `vkCreateWaylandSurfaceKHR`,
    binds its own `wl_seat`/`wl_keyboard` on a private event queue, and polls
    that queue each frame without blocking.
  - The compositor sends key events to every keyboard object of the focused
    client, so the layer sees keys whenever the game has focus. That needs no
    Wine hooks and no `/dev/input`.
  - A `wl_data_device` works the same way for the clipboard. The transfer
    pipe is drained a bit each frame, so the present thread never blocks on
    Wine.
- `src/wl_pointer.cpp`: mouse input. Pointer events go to the surface under
  the cursor. So the layer attaches a transparent subsurface of its own to the
  game's surface, sized to the visible panels, with an input region covering
  just them. Clicks there reach the UI and never the game. With no panel
  visible, the subsurface is unmapped.
- `src/stash_scroll.cpp`: Ctrl + wheel.
  - The layer's own `wl_pointer` on the game's connection gets the game's
    wheel events too. Whole notches come from `axis_discrete`, and touchpad
    scrolling is added up per `wl_pointer` frame.
  - Outside the stash, while the game has keyboard focus, each notch becomes
    an arrow key on the uinput keyboard: pressed in one frame and released in
    the next.
- `src/wl_grab.cpp`: typing into a panel. The game keeps keyboard focus, so
  to type a number the layer briefly takes it:
  - On a Wayland connection of its own, it creates a 1x1 transparent
    layer-shell surface on the overlay layer. The surface has exclusive
    keyboard interactivity and an empty input region, so clicks still reach
    the panels and the game.
  - Destroying the surface gives the keyboard back.
- `src/uinput_kbd.cpp`: a virtual keyboard on `/dev/uinput` that sends
  Ctrl+C and, for stash scroll, the arrow keys. Wayland has no other way to inject keys into a window, and the
  compositor treats the device as a normal keyboard. It needs write access to
  `/dev/uinput`: systemd's uaccess ACL usually grants it to the logged-in
  user; otherwise the user needs the `input` group.
- `src/price_check.cpp`: the flow from hotkey to price.
  1. On the hotkey, save the clipboard: all its mime types, for up to 150 ms.
  2. Send Ctrl+C.
  3. Wait up to 1 s for a new clipboard transfer. Only clipboard changes
     right after a Ctrl+C/Ctrl+D count, so stale clipboard contents never pop
     up.
  4. Restore the saved clipboard. It goes through `ext-data-control-v1`, which
     needs no input serial, or else through the data device.
  5. Parse the item (`src/item_text.cpp`) and show it.

  The item text then goes to the daemon through `src/daemon_client.cpp`,
  which also never blocks the frame.
- `src/ui.cpp`, `src/rml_renderer.cpp`, `assets/ui/`: the UI.
  - RmlUi documents (`.rml`), styled by `theme.rcss`, are bound to a data
    model that mirrors the price check's state.
  - They're drawn by a small Vulkan renderer of our own, which also linearises
    colours for sRGB swapchains.
  - Fonts and UI files are built into the `.so` with `#embed`, since the
    layer can't see host paths from inside Steam's container.
  - `theme.rcss` starts with `@define $name value;` lines. RCSS has no
    variables, so the loader substitutes them. This keeps the palette,
    accent included, in one place.

The `.so` exports only the three loader entry points and links libstdc++
statically, so its RmlUi can't clash with another UI library in the process
(for example MangoHud's).

### The daemon

- `daemon/src/main.rs`: the daemon and its socket protocol, described at the
  top of the file.
  - It listens on the abstract Unix socket `@poe-waytrade`. Abstract sockets
    ignore mount namespaces, so the socket can be reached from inside Steam's
    runtime container. Other users are rejected via `SO_PEERCRED`.
  - It's socket-activated: systemd owns `@poe-waytrade`, starts the daemon on
    the first connection, and the daemon exits after 30 minutes idle. Without
    systemd, the `waytrade` launcher runs it for as long as the game runs.
  - The layer pings it when the game starts, so prices are loaded by the
    first lookup.
- `daemon/src/ninja.rs`: loads poe.ninja's PoE1 data for the league at start
  and every 15 minutes, so lookups never wait on the network.
- `daemon/src/trade.rs`, `daemon/src/cdp.rs`: trade search.
  - pathofexile.com is behind Cloudflare, which refuses plain HTTP clients.
    So the daemon uses a real browser: a headless Chromium/Chrome, as-is with
    its own user agent, driven over the DevTools pipe. It makes same-origin
    `fetch()` calls from a page on the site.
  - The browser starts on the first trade search and closes after 10 minutes
    unused.
  - Mod lines map to trade stat ids by their text, with numbers replaced by
    `#`. The stat list is cached daily in `~/.cache/poe-waytrade`.
  - The daemon follows GGG's `X-Rate-Limit-*` headers. It refuses requests
    that would hit a limit rather than risk an IP timeout.
- `daemon/src/item.rs`, `lookup.rs`, `mapcheck.rs`, `settings.rs`: item text
  parsing, price lookup, the map check, and the settings file.

## Building

The layer needs GCC 15 (for `#embed`), CMake, Ninja, the Vulkan headers,
wayland-protocols, `glslc` (shaderc) and FreeType:

```bash
cmake -S . -B build -G Ninja && cmake --build build
```

The daemon:

```bash
cd daemon && cargo run --release                 # serve
cargo run --release -- query item.txt            # ask a running daemon
```

Both binaries report the exact build from `git describe`. The daemon prints it
with `--version` and at start. The layer logs it when the game creates its
Vulkan instance.

## Running from the build tree

The build writes a manifest pointing at the built library, so you can test
without installing:

```bash
VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_WAYTRADE=1 vkcube --wsi wayland
```

To run the game by hand, with the daemon from systemd, use these launch
options:

```
ENABLE_POE_WAYTRADE=1 PROTON_ENABLE_WAYLAND=1 %command%
```

The layer is inert without `ENABLE_POE_WAYTRADE=1`.

### Working on the UI without the game

```bash
POE_WAYTRADE_UI_DIR=$PWD/assets POE_WAYTRADE_DEMO_ITEM=item.txt \
  VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_WAYTRADE=1 vkcube --wsi wayland
```

| Variable | Effect |
|---|---|
| `POE_WAYTRADE_UI_DIR=<dir>` | Load `ui/*` from disk and reload them when they change |
| `POE_WAYTRADE_DEMO_ITEM=<file>` | Open the panel on start with this item text, as copied from the game (maps open the map check) |
| `POE_WAYTRADE_DEMO_VIEW=price` | A demo map opens on its price instead |
| `POE_WAYTRADE_DEMO_SEARCH=1` | A demo item searches by itself |
| `POE_WAYTRADE_DEMO_EDIT=<filter index>[:max]` | Start typing into that filter's minimum (or maximum), as if it were clicked |
| `POE_WAYTRADE_DEMO_PRESET=base` | Open a demo item on the Base item preset |
| `POE_WAYTRADE_DEMO_SETTINGS=<0-4>` | Open the settings page on that tab |
| `POE_WAYTRADE_ART_DUMP=<file.ppm>` | Save the pixels each art match sends the daemon (the cursor and cell size in a comment), to see what it compares |
| `POE_WAYTRADE_SNAPSHOT=<file.ppm>` | Save one of the app's rendered frames (after 300 frames), for checking the UI without screenshots |
| `POE_WAYTRADE_TEST_COPY=1` | Do a Ctrl+D two seconds in, without sending keys |
| `POE_WAYTRADE_SOCKET=<name>` | Talk to a daemon on another abstract socket (the layer and `poe-waytrade-daemon query`), to test a new daemon next to the installed one |
| `POE_WAYTRADE_DEBUG=1` | A debug window: focus, last key, clipboard state |
| `POE_WAYTRADE_LOG=<file>` | Also write the layer's log to this file |

`tests/clipboard-kwin.sh [text|image]` uses `POE_WAYTRADE_TEST_COPY` in a
nested, invisible KWin to check that the clipboard is restored.

## Installing (Arch)

```bash
cd packaging/arch && makepkg -si
```

This installs:
- the layer, `/usr/lib/libVkLayer_poe_waytrade.so`, with a manifest in
  `/usr/share/vulkan/implicit_layer.d/`;
- the `waytrade` launcher;
- `/usr/bin/poe-waytrade-daemon` and its systemd user socket and service.

It also enables the socket for all users (`systemctl --global`), so from the
next login on there's nothing to start by hand. For the current login, run
`systemctl --user start poe-waytrade-daemon.socket`. Remove the package with
`pacman -R poe-waytrade`.

To set `POE_LEAGUE` for the socket-activated daemon, use
`systemctl --user edit poe-waytrade-daemon.service`.

## CI and releases

GitHub Actions runs these jobs in Arch containers on every push to main and
every pull request:
- `lint`: rustfmt, and clippy with warnings as errors.
- `test`: the daemon's tests and the `waytrade` launcher.
- `layer`: builds the layer (with ccache, so the vendored RmlUi is rarely
  rebuilt) and checks that it exports only the three Vulkan loader entry
  points.

Pushes to main also run `package`, the full Arch package (layer and daemon),
which takes about 4 minutes, so pull requests don't wait for it.

[AGENTS.md](../AGENTS.md) lists the commands to run them locally, how to
refresh the daemon's test fixtures, and how to bump the version.

To release, bump `pkgver` in `packaging/arch/PKGBUILD` (and the daemon's
version with it, see AGENTS.md), commit, and push a matching tag:

```bash
git tag v0.21.0 && git push origin v0.21.0
```

The release workflow attaches the Arch package and a tarball of the same
files laid out from `/` (for other distros with a recent glibc), with
checksums.
