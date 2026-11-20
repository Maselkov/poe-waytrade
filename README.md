# Waytrade (prototype)

In-frame price checks for Path of Exile on Linux/Wayland.

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
- Ctrl+D keeps your clipboard: what it held (text, images, every format it
  was offered in) is saved before the game copies the item and put back
  right after. The restored content is served by the game's process, like
  `wl-copy` does; KDE's Klipper keeps it once the game exits. The item text
  still shows up in clipboard managers' history: they see the game's copy,
  and Klipper can't be told to forget a single entry.
- Like APT, the panel sits right next to PoE's inventory when you check an
  item there (or next to the stash for items on the left half of the
  screen). PoE's side panels are 986/1600 of the window height wide.

**Maps** (and invitations, heist contracts and blueprints) open the map
check instead, with the same keys: a one-line verdict at the top ("1 deadly
mod · 1 warning · 5 unmarked", "No dangerous mods · 3 unmarked"; unmarked mods
are counted so they don't get skimmed past), the map's rarity, pack size and
"More ..." bonuses, its poe.ninja price (for a rare map, the white map's), and
its mods. Click a mod to cycle it through deadly → warning → good → unmarked;
marks are per profile (I/II/III) and saved in
`~/.config/poe-waytrade/map-mods.json` (moved there from
`~/.config/poe-overlay/` the first time, if you have marks from before the
rename). Reflect, no regen, less recovery, −max res, cannot leech, player
curses and a few others come pre-marked.

**Price check** in the map check switches to the map's trade search: its base
("Map (Tier 16)", which names the tier), its More currency/scarabs/maps/cards
bonuses (ticked), pack size and rarity (unticked), and blight. **Map check**
switches back; each view keeps its state, and switching locks the panel.
Expedition logbooks open on their price, with the map check one click away.
Alt+D, the old map check key, still works and now does the same as
Ctrl+Alt+D.

The game keeps keyboard focus (the panel is part of its window), so the
overlay never uses keys PoE does; in particular there's no Esc to close.

When the game starts, a small notice rises in under the login box saying the
overlay is ready (or that prices are still loading, or that the price daemon
isn't answering) and naming the price check and settings keys. It goes away
after six seconds, and clicks go through it.

**Settings**: Ctrl+Alt+S, or the gear at the top left of a panel. Four tabs:

- **Hotkeys**: click a key, press the new combination. All four can be
  rebound (Alt+D can also be turned off); Ctrl+C stays the copy key. A key
  without Ctrl or Alt gets a warning first, since the game sees it too, and a
  combination another action has swaps the two. Holding the quick look
  key's modifiers (Ctrl, by default) keeps a quick look open.
- **Price check**: the league ("Current" follows poe.ninja into each new
  league), which listings a search starts with (instant buyout, + in person,
  any), how far below your roll a ticked filter starts (10% by default), and
  the panel size.
- **Map check**: the active profile, and resetting its marks.
- **About**: versions, where the logs are, whether typing into panels works.

The daemon keeps them in `~/.config/poe-waytrade/settings.json` and applies
each change at once. `POE_LEAGUE` and `POE_WAYTRADE_UI_SCALE` still win over
the page.

Priced: currency, fragments, essences, divination cards, omens, oils,
fossils, tattoos and other exchange items; uniques (incl. 5L/6L and
unidentified); skill gems (level/quality/corruption); maps, invitations,
beasts; Wickerman limbs (Viscera, matched by their implicits); white and
magic cluster jewels (by enchant, passive count and item level bracket);
plain bases of item level 82+ (by item level and influences).

Currency and other exchange items are priced from poe.ninja, whose figures
come from the in-game Currency Exchange, where currency actually trades. The
trade site's bulk exchange (whisper-and-trade listings from stash tabs) is
left out: APT marks it as legacy for anything on the Currency Exchange.

Skill gems get a trade search too, next to their poe.ninja price: gem level
(ticked at max level) and quality (ticked from 16%, or 20% on a transfigured
gem), matching corruption, and transfigured gems searched as the trade site
knows them. Like APT, level and quality are minimums: a better gem listed
for less is competition (an uncorrupted gem never meets one, since going
past 20 takes a corruption). Listings show each gem's level and quality on its row.

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
corrupted items count (uncorrupted items exclude them by default). Searches
count **instant buyout** listings only by default (merchant tabs, which
can't be price-fixed: the seller can't list low and refuse to sell); the
button next to Search switches to instant buyout plus in-person sellers, or
any listing. Nothing is
searched until you press **Search**, since every search counts against GGG's
rate limit; changing anything afterwards marks the listings as out of date
until you search again.

Every price check copies the item with Ctrl+C, which PoE fills with the
advanced description: each mod's affix name, tier and roll range. Every filter with a range has a slim slider strip under
it, so ticking a row never moves the others: unticked, it's a faint track
marking where your item's value sits; ticked, it's a two-handle slider,
like APT's: the left handle sets the minimum, the right one a maximum (at the
far right: none). The minimum defaults to your roll minus 10% (the slider
reaches down to it even below the tier), and a roll at the top of its tier defaults to exactly that value
(marked with a gold star). Scrolling the mouse wheel over a filter's value
moves its minimum one step. **Clicking the value types into it**: the
overlay takes the keyboard from the game while you type (so digits don't
drink flasks), Tab switches between minimum and maximum, Enter applies and
ticks the filter, Esc or a click in the game cancels, and the keyboard goes
back to the game. This needs a compositor with layer-shell (KDE Plasma,
Sway, Hyprland and other wlroots ones, COSMIC); on GNOME or gamescope the
panel says typing isn't available, and the wheel and sliders still work. Weapons
also get physical/elemental DPS, attack speed and critical strike chance
filters from their properties; armour pieces get armour, evasion, energy
shield, ward and block filters (unticked), scaled to 20% quality the way the
trade site searches them. 5- and 6-links get a links filter, ticked except
on a rare 5-link, and an unlinked 6-socket gets a sockets filter.

Every filter is listed, grouped under headings (weapon/armour/base item,
enchant, implicit, totals, mods); clicking a row ticks it, and ticked rows
get an accent bar. Each mod's affix tier from the advanced copy sits in a
column on the right (T1 gold, T2 silver, T3 bronze, lower ones grey), next
to its value.
Fractured mods count in the totals and also get a filter of their own that
searches them as fractured, shown in PoE's fractured gold.

Like APT, unfinished bases with crafting value (fractured, synthesised,
influenced, cluster or magic jewels, or item level 82+, without crafted
mods, corruption or 20% quality) get a **Base item** button: it searches the
exact base with a minimum item level, the fractured/synthesised/influence
flags, fractured mods, enchants, implicits (unless influenced or fractured)
and, on magic items, tier 1–2 mods. Magic items' bases are worked out from
the advanced copy's affix names ("Fecund Paladin Crown of the Genius" →
Paladin Crown), so they can be searched by exact base too.

Any non-unique 5- or 6-link also gets the Base item button, with the links
ticked. White and magic ones open in it, since they're worth their links and
base rather than their mods; for these, item level and implicits start
unticked unless the base also has crafting value.

Mods that are one of a list rather than a number are searched as exactly
that, ticked: a cluster jewel's "Added Small Passive Skills grant" (two-line
grants included), an anointed notable ("Allocates Charisma"), Forbidden
Flame/Flesh's ascendancy notable. A cluster jewel's passive count is
searched exactly and its jewel sockets as a minimum. Searing Exarch and
Eater of Worlds items get a filter for their implicit's tier (Lesser to
Perfect, from advanced copy), or for the eldritch item flag, ticked on the
Base item preset.

After a search, the panel shows a one-line estimate (the median of the 10
cheapest listings, with the price range and a "Low value" tag under 5c), then
the listings grouped by price with currency icons, how many there are, and
how long ago they were listed, plus a button to open the same search in your
browser. Hovering a listing shows the listed item (name, item level, DPS and
mods with their tiers, like PoE's tooltip) beside the panel.

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
- `src/price_check.cpp`: hotkey → save the clipboard (all its mime types,
  up to 150 ms) → Ctrl+C → wait up to 1 s for a new clipboard transfer →
  restore the saved clipboard (through `ext-data-control-v1`, which needs no
  input serial, or the data device) → parse (`src/item_text.cpp`) → show. Only clipboard changes right
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
- `src/wl_grab.cpp`: typing into a panel. The game keeps keyboard focus, so
  to type a number the layer briefly takes it: on a Wayland connection of
  its own, a 1x1 transparent layer-shell surface on the overlay layer with
  exclusive keyboard interactivity and an empty input region (clicks still
  reach the panels and the game). Destroying it gives the keyboard back.
- `daemon/` (Rust): `poe-waytrade-daemon` listens on the abstract Unix socket
  `@poe-waytrade` (reachable from inside Steam's runtime container, since
  abstract sockets ignore mount namespaces; other users are rejected via
  `SO_PEERCRED`). It loads poe.ninja's PoE1 data for the current league at
  start and every 15 minutes, so lookups never wait on the network. The
  protocol is described at the top of `daemon/src/main.rs`.
  It's socket-activated: systemd owns `@poe-waytrade`, starts the daemon on
  the first connection and the daemon exits after 30 minutes idle. The layer
  pings it when the game starts, so prices are loaded by the first lookup.
- Trade search (`daemon/src/trade.rs`, `daemon/src/cdp.rs`): pathofexile.com
  is behind Cloudflare, which refuses plain HTTP clients. Like Awakened PoE
  Trade (Electron's Chromium), the daemon uses a real browser: a headless
  Chromium/Chrome, as-is with its own user agent, driven over the DevTools
  pipe, making same-origin `fetch()` calls from a page on the site. It starts
  on the first rare price check and closes after 10 minutes unused. Mod
  lines map to trade stat ids by their text with numbers replaced by `#`
  (stat list cached daily in `~/.cache/poe-waytrade`). The daemon follows
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
VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_WAYTRADE=1 vkcube --wsi wayland
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

This installs the layer (`/usr/lib/libVkLayer_poe_waytrade.so` plus a manifest
in `/usr/share/vulkan/implicit_layer.d/`), the `waytrade` launcher,
`/usr/bin/poe-waytrade-daemon` and its systemd user socket + service, and enables the socket for all users
(`systemctl --global`), so there is nothing to start by hand from the next
login on (`systemctl --user start poe-waytrade-daemon.socket` for the current
one). Remove with `pacman -R poe-waytrade`. The daemon follows poe.ninja's
current softcore league unless another is picked in the settings; setting
`POE_LEAGUE` in the service (`systemctl --user edit poe-waytrade-daemon.service`)
overrides both.

## CI and releases

GitHub Actions runs three jobs in Arch containers (the layer needs GCC 15
for `#embed`) on every push to main and every pull request: `lint`
(rustfmt, clippy with warnings as errors), `test` (the daemon's tests, the
`waytrade` launcher) and `package` (the Arch package, which builds the
layer and the daemon, and a check that the layer exports only the three
Vulkan loader entry points). [AGENTS.md](AGENTS.md) lists the commands to
run them locally.

To release, bump `pkgver` in `packaging/arch/PKGBUILD`, commit, and push a
matching tag:

```
git tag v0.12.0 && git push origin v0.12.0
```

The release workflow attaches the Arch package and a tarball of the same
files laid out from `/` (for other distros with a recent glibc), with
checksums.

## Use with Path of Exile

Steam launch options, like MangoHud's:

```
waytrade %command%
```

`waytrade` turns the layer on (`ENABLE_POE_WAYTRADE=1`; it's inert
otherwise), uses Proton's Wayland driver (`PROTON_ENABLE_WAYLAND=1`), and
runs the price daemon for as long as the game does, unless one is already
serving (e.g. from the systemd socket), so it works without systemd too.
The daemon's log is in `~/.local/state/poe-waytrade/daemon.log`; add
`POE_WAYTRADE_LOG=/tmp/poe-waytrade.log` before `waytrade` for the layer's.

The same by hand: `ENABLE_POE_WAYTRADE=1 PROTON_ENABLE_WAYLAND=1 %command%`,
with the daemon from systemd. With `PROTON_ENABLE_WAYLAND=0` the game runs
on XWayland: the panel still draws, but hotkeys and the clipboard are not
implemented for X11 yet.

`POE_WAYTRADE_DEBUG=1` adds a debug window (focus, last key, clipboard state).

Working on the UI without the game:

```bash
POE_WAYTRADE_UI_DIR=$PWD/assets POE_WAYTRADE_DEMO_ITEM=item.txt \
  VK_ADD_IMPLICIT_LAYER_PATH=$PWD/build/layer ENABLE_POE_WAYTRADE=1 vkcube --wsi wayland
```

`POE_WAYTRADE_UI_DIR` loads `ui/*` from disk and reloads them when they
change; `POE_WAYTRADE_DEMO_ITEM` opens the panel on start with that item text
(as copied from the game; maps open the map check). `POE_WAYTRADE_UI_SCALE`
scales the whole UI. `POE_WAYTRADE_SNAPSHOT=<file.ppm>` saves one of the app's own rendered frames
(after 300 frames) and `POE_WAYTRADE_DEMO_SEARCH=1` makes a demo item search by
itself, for checking the UI without screenshots. `POE_WAYTRADE_DEMO_EDIT=<filter index>` starts
typing into that filter's value, as if it were clicked. `POE_WAYTRADE_DEMO_SETTINGS=<0-3>` opens the
settings page on that tab. `POE_WAYTRADE_DEMO_PRESET=base` opens a demo item on
the Base item preset. `POE_WAYTRADE_TEST_COPY=1`
does a Ctrl+D two seconds in without sending keys; `tests/clipboard-kwin.sh
[text|image]` uses it in a nested, invisible KWin to check that the
clipboard is restored. `POE_WAYTRADE_SOCKET=name`
makes the layer (and
`poe-waytrade-daemon query`) talk to a daemon on another abstract socket, for
testing a new daemon next to the installed one.

## Known limitations / next steps

- Hotkeys still reach the game too (it keeps the keyboard), so they should be
  combinations PoE doesn't use. Their labels name US keys: a hotkey is a
  physical key, wherever the layout puts its letter.
- Mouse input under fractional scaling assumes the app renders at full
  resolution if it runs under Wine, at logical size otherwise;
  `POE_WAYTRADE_SCALE` overrides the pixel-per-logical-unit ratio.
- No X11 input path (would be `XQueryKeymap` polling like MangoHud).
- Only one device gets the overlay (RmlUi is a process-wide singleton).
- HDR (10-bit/PQ) swapchains wouldn't be colour-corrected (sRGB ones are).
  PoE1 has no HDR mode, so this only matters for PoE2.
- Magic items' exact base is only known from advanced copy's affix names;
  with plain Ctrl+C a magic item searches its category.
- Trade search has no socket colour filters (APT has none either).

## License

poe-waytrade is free software under the GNU General Public License, version
3 or (at your option) any later version; see [LICENSE](LICENSE).

Bundled third-party code keeps its own license: `third_party/rmlui` is RmlUi 6.3 core (MIT); `third_party/stb` has
stb_image (public domain). `protocols/wlr-layer-shell-unstable-v1.xml` is
wlroots' layer-shell protocol (its own permissive licence, in the file).
`assets/fonts` has Inter and
Cinzel, both under the SIL Open Font License (licence files alongside).
