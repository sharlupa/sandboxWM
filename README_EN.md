# sandboxWM

[Русская версия](./README.md)

**sandboxWM** is an experimental Rust Wayland compositor. A normal BSP/Dwindle tiling desktop turns into a physical sandbox on the fly: windows fall, collide and rotate. You can draw shelves, glue, trampolines and ropes with the cursor, link windows with welds, springs and ropes, squish windows like jelly and cut them into pieces with a knife. All of it is saved when you quit.

> ⚠️ **Status: 0.2.0, experimental.** Every item of the [concept](./sandboxWM_concept_EN.md) is implemented, but the project is young: expect rough edges, especially with unusual clients.

**Developed with AI:** this project and its concept are developed with the help of AI coding assistants.

## Concept → implementation

| Concept item | Status | How it works |
|---|---|---|
| §1 Infinite canvas, gravity, floor | ✅ | rapier2d, windows are dynamic bodies, the floor sits at the bottom of the screen, camera zooms around the cursor |
| §1 Mode 1: normal gravity | ✅ | `Super+F1` |
| §1 Mode 2: windows smoothly line up into tiling | ✅ | `Super+F2` / `Super+G`, animated in both directions; going back puts windows where they were |
| §1 Mode 3: zero gravity for one window | ✅ | `Super+F3`, the body becomes kinematic: it hangs in place and can't be pushed, but can still be dragged and spun |
| §1 Mode 4: zero gravity for everything | ✅ | `Super+F4` |
| §2 Drawing lines: steel, glue, trampoline, rope | ✅ | `Super+P`; chains of capsule colliders, a rope is a chain of hinged segments |
| §2 Glue lets go when yanked with the mouse | ✅ | The bond is a fixed joint that breaks above `detach_force` |
| §2 Rope falls and dangles, a window can hang from it | ✅ | The start is pinned; drag a window to the free end and it hooks on |
| §3 Session persistence | ✅ | Lines, joints, windows, camera and mode; autosave, apps are relaunched on start |
| §4 Weld, spring and rope between windows | ✅ | `Super+E/S/R`, then click two windows; a rope passes through the windows it links |
| §5 Soft windows (jelly, stretching, squashing) | ✅ | `Super+J`, `Super+Shift+LMB`; PBD mesh + a mesh shader over a window snapshot |
| §6 Inverse projection of input | ✅ | Clicks and text selection go through rotation, zoom, slicing and the deformed mesh |
| §7 Slicing a window with a knife | ✅ | `Super+X`, swipe across a window; the parts are crops of one buffer and the app keeps working as a whole |

Beyond the concept: workspaces (5 by default, each with its own tiling/physics), fullscreen windows in both modes (games: relative-pointer and pointer-constraints for mouse capture), bars over wlr-layer-shell (waybar, swaybg, mako, wofi) and ext-workspace (the waybar `ext/workspaces` module), XWayland (Steam, Discord and other X11 clients), DMA-BUF, wlr-screencopy (OBS via xdg-desktop-portal-wlr), wlr-output-management (read-only), clipboard and primary selection, `wp_viewporter`, server-side decorations, VT switching, render on demand.

## Controls

Every shortcut uses `Super`. Keys are configurable in `[controls]`.

### General

| Key | Action |
|---|---|
| `Super+Enter` | Terminal (default `kitty`) |
| `Super+W` | Close the focused window |
| `Super+Q` / `Super+Esc` | Quit (the session is saved) |
| `Super+G` | Tiling ↔ physics (animated) |
| `Super+F` | Toggle fullscreen for the focused window (tiling and physics; games can request it themselves) |
| `Super+1…9`, `Super+0` | Workspace 1–10 (how many there are: `[workspaces] count`, 5 by default) |
| `Super+Shift+1…9`, `Super+Shift+0` | Move the focused window to a workspace |
| `Super+Arrows` | Tiling: focus the neighbouring window in that direction; physics: move the camera |
| `Super+LMB` + drag | Tiling: resize tile ratios; physics: drag a window by the grabbed point (it swings and can be thrown) |
| `LMB` | Focus and normal interaction |
| `Ctrl+Alt+F1…F12` | Switch VT (TTY/DRM) |
| `Super+F5` | Save the session now |

### Physics and camera

| Key | Action |
|---|---|
| `Super+F1` | Mode 1: normal gravity (enters physics) |
| `Super+F2` | Mode 2: gravity off, windows line up into tiling |
| `Super+F3` | Mode 3: zero gravity for the focused window (toggle) |
| `Super+F4` | Mode 4: zero gravity for everything (toggle) |
| `Super+A` / `Super+D` | Hold to spin the focused window |
| `Super+wheel`, `Super+-` / `Super+=` | Zoom around the cursor |
| `Super+0` | Reset the camera (when there are fewer than 10 workspaces) |
| `Super+C` | Gather all scattered windows into view: a grid on the floor, the camera adjusts; all joints, glue and rope hooks are released |
| Middle button + drag | Pan the camera |

### Tools

| Key | Action |
|---|---|
| `Super+P` | Draw lines. `LMB` draws, `RMB` erases the line under the cursor, `1`–`4` or `Tab` pick steel/glue/trampoline/rope, `Delete` erases all lines, `Esc` exits |
| `Super+X` | Knife: swipe `LMB` across a window. A horizontal stroke cuts horizontally, a vertical one vertically |
| `Super+E` / `Super+S` / `Super+R` | Weld / spring / rope: click the first window, then the second (click points are the anchors) |
| `Super+U` | Remove joints, glue and rope hooks from the focused window |
| `Super+J` | Toggle the focused window between soft and rigid |
| `Super+Shift+LMB` + drag | Pull a window by its edge to stretch or squash it (it becomes soft) |
| `Super+Z` | Restore the shape and re-join a sliced window |

## Build and run

You need Rust (edition 2024) and these system libraries: `libwayland`, `libxkbcommon`, `libinput`, `libudev`, `libseat`, `libgbm`, `libEGL`/`libGLESv2`, `libdrm`; plus `Xwayland` for X11 apps.

```sh
# Debian/Ubuntu
sudo apt install libudev-dev libinput-dev libseat-dev libgbm-dev libegl-dev \
    libwayland-dev libxkbcommon-dev libdrm-dev libgles-dev xwayland

cargo build --release
./target/release/sandboxWM
```

The backend is picked automatically: without `WAYLAND_DISPLAY` and `DISPLAY` (started from a TTY) it runs native DRM/KMS via libseat; otherwise it opens a nested Winit window inside the current session.

Tests: `cargo test`. The mesh-renderer GL tests use software EGL (Mesa llvmpipe) and are skipped when it is missing.

## Configuration

The first directory found is used: `$SANDBOXWM_CONFIG`, `$XDG_CONFIG_HOME/sandboxWM/`, `~/.config/sandboxWM/`, `./config/`. Every `*.conf` in it is TOML, merged table by table: a section can continue in another file, and a broken file is skipped with a message instead of resetting the others.

| File | Sections |
|---|---|
| `sandboxWM.conf` | `[physics]` (gravity, floor, density, friction, zoom…), `[controls]` |
| `gravity.conf` | `[gravity_modes]`: transition duration, zero gravity |
| `drawing.conf` | `[drawing]`, `[materials.steel/glue/trampoline/rope]` |
| `joints.conf` | `[joints.weld/spring/rope]` |
| `softbody.conf` | `[soft_body]`: softness, squash, plasticity, mesh, inverse projection |
| `slicing.conf` | `[slicing]` |
| `session.conf` | `[session]`: autosave, path, TOML/JSON format, relaunching apps |
| `window.conf` | `[window_props]` and per-app overrides: `[window_props.apps."org.gnome.Nautilus"] weight = 2.0, softness = 0.3, floating = true` |
| `rendering.conf` | `[rendering]`: background colour, window borders (width, normal and active colour) |
| `workspaces.conf` | `[workspaces]`: `count` — number of workspaces (1–32, 5 by default) |

The session keeps every workspace (windows come back to their own workspace) and is stored in `$XDG_STATE_HOME/sandboxWM/session.toml` by default (with a `.bak` next to it); panics are logged to `$XDG_STATE_HOME/sandboxWM/panic.log`.

## Bars (waybar) and workspaces

sandboxWM supports `wlr-layer-shell`: waybar, swaybg, mako and wofi/fuzzel/rofi-wayland work like they do in sway/Hyprland. A bar reserves its space (`exclusive zone`): tiling lays windows out below it, and the physics floor sits above a bottom bar. The `top` layer (a normal bar) hides under a fullscreen window, `overlay` does not.

Workspaces are exposed to bars over `ext-workspace-v1`: in waybar ≥ 0.12 that is the `ext/workspaces` module (clicking switches workspace). Example `~/.config/waybar/config`:

```json
{
  "layer": "top",
  "position": "top",
  "modules-left": ["ext/workspaces"],
  "modules-center": ["clock"],
  "ext/workspaces": { "format": "{name}", "on-click": "activate" }
}
```

Start it alongside the WM, e.g. from a terminal: `waybar &`.

## Screen recording (OBS)

OBS captures the screen through `xdg-desktop-portal-wlr` and PipeWire (the "Screen Capture (PipeWire)" source). sandboxWM sets `XDG_CURRENT_DESKTOP=sandboxWM` and restarts the portals itself; you need `~/.config/xdg-desktop-portal/sandboxWM-portals.conf`:

```ini
[preferred]
default=wlr;gtk
```

If OBS shows one frame and freezes and `pacman -Q xdg-desktop-portal-wlr` says 0.8.3 or 0.8.4, it is a bug in the portal itself: its PipeWire stream drives the graph but never asks for the next frame after the first one (fixed in the git version, where the portal starts the next PipeWire cycle after each frame). Options:
- install the git version of the portal (`xdg-desktop-portal-wlr-git` in the AUR), or build it and run it with `SANDBOXWM_PORTAL_BIN=/path/to/xdg-desktop-portal-wlr`;
- capture without the portal using the [wlrobs](https://hg.sr.ht/~scoopta/wlrobs) OBS plugin, which reads the screen directly over wlr-screencopy.

If the picture freezes or the cursor is missing:
- `SANDBOXWM_DEBUG_SCREENCOPY=1` makes sandboxWM print every capture frame (`capture`, `copy…`, `ready`);
- `SANDBOXWM_SCREENCOPY_DMABUF=0` turns off DMA-BUF capture (shm only);
- `SANDBOXWM_PORTAL_LOG=~/xdpw.log` (when started from a TTY): sandboxWM starts `xdg-desktop-portal-wlr -l DEBUG` itself and writes its log to that file (by hand: `/usr/lib/xdg-desktop-portal-wlr -r -l DEBUG`, sometimes under `/usr/libexec/…`).

## How it works

- `src/main.rs` — startup, backend selection, the Winit loop.
- `src/backend_drm.rs` — DRM/KMS, EGL/GBM, libinput, cursor, screencopy.
- `src/state.rs` — compositor state, tiling, focus, Wayland handlers (xdg-shell, popups with grabs, clipboard, viewporter…).
- `src/sandbox.rs` — the physics sandbox on top of the state: window pieces, tools, gravity modes, transitions, inverse projection, overlay, session.
- `src/physics.rs` — the rapier2d world: window pieces, lines and materials, ropes, joints, glue, dragging, contact hooks.
- `src/softbody.rs` — a window's soft body (PBD mesh with plasticity and inertial pseudo-forces).
- `src/render.rs` — render elements: rotated/cropped window pieces, frame assembly for both backends.
- `src/mesh_render.rs` — window snapshots and GLES drawing of the deformed mesh.
- `src/overlay.rs` — lines, ropes, joints, tool previews and the floor in one draw call; damage clipping.
- `src/session.rs` — session save/load, window matching, app relaunch.
- `src/cursor.rs` — the DRM-mode cursor (client surfaces, xcursor theme, hardware plane).
- `src/workspace.rs` — workspaces; `src/fullscreen.rs` — fullscreen windows; `src/layer_shell.rs` — bars and other layer surfaces; `src/ext_workspace.rs` — workspaces for bars.
- `src/tiling.rs` — the BSP/Dwindle tree; `src/xwm.rs` — XWayland; `src/screencopy.rs`, `src/output_manager.rs` — wlr protocols.

Differences from the concept's original technical plan:
- Rendering uses GLES (Smithay) with custom shaders for the mesh and the overlay, not a separate wgpu/glow renderer.
- Slicing cuts along the window's axes: each part is a rectangular crop of the same buffer, like `wp_viewporter`. The `wp_viewporter` protocol itself is also supported for clients.
- X11 windows don't rotate in physics mode: XWayland can't express rotated geometry, so Steam/CEF clicks would drift.

## Known limitations

- One monitor; scale 1 (HiDPI buffers are drawn downscaled).
- A deformed soft window is drawn from a snapshot and redraws its bounding box every frame while it wobbles.
- Relaunching apps from the session works for ordinary processes; complex ones (Steam, CEF wrappers, Snap after an update) may not come back from their argv.

## Stack

Rust 2024 · Smithay 0.7 · rapier2d 0.34 · GLES/EGL · GBM/DRM · libinput · XWayland · serde/TOML/JSON
