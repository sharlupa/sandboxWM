# sandboxWM

[Русская версия](./README.md)

**sandboxWM** is an experimental Rust Wayland compositor. It can switch a conventional BSP/Dwindle desktop into a physical sandbox where windows have mass, fall, collide, and rotate.

> ⚠️ **Status: early version (0.1.0).** This is a working prototype. Drawing tools, persistence, window joints, deformation, and slicing describe the project direction rather than shipping features.

## Feature status

| Feature | Status |
|---|---|
| Wayland compositor, BSP/Dwindle, and DRM/Winit | Implemented |
| Window physics: gravity, collisions, floor, drag, rotation, and camera | Implemented |
| Physics camera zoom and panning | Implemented |
| wlr-screencopy screen capture | Basic implementation |
| wlr-output-management | Read-only output state |
| Cursor line drawing and physical materials | Not implemented |
| Session persistence and window joints | Not implemented |
| Soft windows, deformation, and slicing | Not implemented |
 

## Implemented

- A **Smithay 0.7** Wayland compositor with `xdg_shell`, SHM, seat/input, outputs, server-side decorations, and clipboard support.
- Two runtime backends:
  - nested **Winit** inside an existing graphical session;
  - native **DRM/KMS** from a TTY using libseat, GBM/EGL, udev, and libinput.
- BSP/Dwindle tiling with automatic splits, focus switching, and mouse ratio resizing.
- A **rapier2d** physics mode: gravity, a wide static floor, collisions, damping, CCD, collider-size synchronization, non-teleport drag, and an infinite-canvas camera.
- Window rotation using GLES texture transforms, rotation-aware hit testing, and `Super+A` / `Super+D` spin controls.
- Damage-gated rendering, DMA-BUF client-buffer import, and a software cursor in DRM mode.
- Basic `wlr-screencopy` and read-only `wlr-output-management` protocol globals.
- A visible floor line in DRM mode. The decorative dot grid was removed because its hundreds of render elements caused micro-stutters near the floor.

## Controls

| Shortcut | Action |
|---|---|
| `Super+Enter` | Launch terminal (default: `kitty`) |
| `Super+Q` / `Super+Esc` | Quit WM |
| `Super+W` | Close focused window |
| `Super+G` | Toggle tiling ↔ physics |
| `Super+A` / `Super+D` | Hold to spin the focused window in physics mode |
| `Super+Arrow keys` | Tiling: change focus; Physics: move camera |
| `Super+-` / `Super+=` | Physics: zoom camera out / in |
| `Super+0` | Physics: reset camera position and zoom |
| `Super+wheel` | Physics: zoom camera (only while Super is held) |
| Middle mouse button + drag | Physics: pan camera |
| `Super+LMB` + drag | Tiling: resize split; Physics: drag a window body |
| `LMB` | Focus and interact with a window |
| `Ctrl+Alt+F1…F12` | Switch VT in TTY/DRM mode |

## Build and run

Rust plus Wayland/DRM/GBM/EGL system libraries are required; native mode also requires libseat.



The backend is selected automatically: without both `WAYLAND_DISPLAY` and `DISPLAY`, sandboxWM starts DRM/KMS; otherwise it starts Winit.

## Configuration

At startup, sandboxWM uses the first directory it finds:

1. `$SANDBOXWM_CONFIG`
2. `$XDG_CONFIG_HOME/sandboxWM/`
3. `~/.config/sandboxWM/`
4. `./config/` in the repository root

All `*.conf` files in that directory are combined as TOML. `[physics]` and `[controls]` in `config/sandboxWM.conf` are live. The other schemas (`drawing`, `materials`, `joints`, `soft_body`, `slicing`, `session`, and `window_props`) already have example settings but are not wired into runtime behavior yet.

## Project layout

- `src/main.rs` — entry point, backend selection, event loop.
- `src/backend_drm.rs` — DRM/KMS, EGL/GBM, libinput, cursor, floor, and screencopy.
- `src/state.rs` — application state, layouts, physics, camera, and Wayland handlers.
- `src/input.rs` — keyboard, pointer, and shortcuts.
- `src/physics.rs` — rapier2d wrapper.
- `src/render.rs` — GLES render elements and rotated windows.
- `src/tiling.rs` — BSP/Dwindle tree.
- `src/screencopy.rs`, `src/output_manager.rs` — wlr protocol implementations.
- `config/` — modular TOML configuration.

## Concept and roadmap

See [sandboxWM_concept_EN.md](./sandboxWM_concept_EN.md) for the long-term vision: steel/glue/trampoline/rope drawing tools, session persistence, Weld/Spring/Rope joints, soft windows, inverse input projection, and `wp_viewporter` slicing.

## Stack

Rust 2024 · Smithay 0.7 · rapier2d 0.34 · GLES/EGL · GBM/DRM · libinput · serde/TOML
