# sandboxWM

[Russian version](./README.md)

## Project Philosophy

Conventional window managers are rigid, predictable, and boring. Windows are just rectangles that you drag, resize, and stack. sandboxWM breaks this pattern.

The idea is to turn the desktop into a **physical sandbox**. Windows cease to be static rectangles — they gain weight, inertia, and gravity. A new window **falls** from the top and lands on the floor or on other windows. You can **draw** a shelf with your cursor, and the windows will stand on it. You can **glue** two windows together so they become a single entity. You can **slice** an application into pieces and work with the fragments individually.

sandboxWM is an experiment at the intersection of a Window Manager and a physics engine. It answers the question: *“What if windows were not just rectangles, but physical objects?”*. The desktop becomes a **playground** rather than a window management tool, where the user decides how their workspace looks.

A detailed description of the final vision is available in [`sandboxWM_concept_EN.md`](./sandboxWM_concept_EN.md).

> ⚠️ **Status:** Early version (`0.1.0`). A basic tiling WM based on Smithay with optimized rendering and a software cursor. Physics mode (Phase 1+): `Super+G` turns windows into dynamic rapier2d bodies with gravity, a floor, collisions, and **rotation**; holding `Super+A` / `Super+D` spins the focused window, and on release the angle stays put while the body becomes free again. Drawing, deformations, joints, and slicing are still **goals** (see the "Current Implementation vs. Final Concept" section).

---

## AI Co-Creation / Developed with AI

This project is actively developed in collaboration with AI coding assistants (neural networks). All design structures, implementation details, and bug fixes (including state logic, compositor configuration, and physics integration plan) are co-authored by human and AI in a pair-programming fashion.

---

## Tech Stack

| Layer | Technology | Currently Used |
|------|------------|:-------------------:|
| Language | Rust | ✅ |
| Wayland Compositor | Smithay 0.7 | ✅ |
| Physics Engine | rapier2d | ✅ Phase 1+ (gravity, bodies, rotation, camera) |
| Rendering | wgpu / glow | 📋 Planned (currently GLES via Smithay) |

---

## Final Concept (from `sandboxWM_concept_EN.md`)

### 1. Infinite Tiling and Gravity
The desktop is not a bounded rectangle but an infinite space with gravity and a physical floor. A new window falls from the top until it hits the floor or other windows. Windows have weight and inertia.

**Gravity modes, switchable on the fly:**
1. Standard — gravity enabled.
2. Complete freeze — gravity is turned off, and windows smoothly align into a strict tiling layout.
3. Selective zero gravity — gravity is disabled for **one** specific window (it floats in the air), while others continue to fall.
4. Total weightlessness — zero gravity for all windows.

*Technical detail:* each window is a `RigidBody` in rapier2d; switching the body type from `Dynamic` to `Kinematic`/`Fixed` locks the window.

### 2. Interactive Drawing (Physical Lines)
Lines drawn with the cursor on the desktop become physical obstacles that windows interact with. Line materials:
- **Steel (Floor)** — a solid shelf for stacking windows.
- **Glue** — a sticky line where windows stick upon contact; pulling them away requires force.
- **Trampoline** — an elastic line from which windows bounce off.
- **Rope** — a dynamic line that falls under gravity and swings; windows can be hung on it.

*Technical detail:* a chain of colliders generated along the cursor path in the physics engine. Friction represents glue, restitution represents the trampoline. Ropes are built from short rigid bodies connected by `BallJoint`s.

### 3. Persistence (Session Saving)
On system reboot or WM exit, the entire workspace is saved: drawn shelves/trampolines/ropes and windows with their coordinates and physical properties. Everything is restored to its exact place on the next startup.

*Technical detail:* `serde` serializes session state to a JSON/TOML file; on startup, the geometry is parsed and restored in rapier2d.

### 4. Joining Windows (Joints)
- **Welding** — two windows (e.g., browser and terminal) are fused into a monolith, moving and falling together (`WeldJoint`).
- **Spring** — windows are connected by a spring (visible or invisible). Pulling one window away makes it snap back.
- **Rope** — physical connection via rope. By default, the rope passes through joined windows.

### 5. Full Deformation of Windows (Soft Bodies)
Windows are no longer rigid rectangles. You can pull an edge to stretch it into a triangle, a circle, or crumple it. Upon a hard impact on the floor, a window might temporarily squash like jelly. Text and UI elements warp along with the window's shape.

*Technical detail:* windows are rendered via wgpu/glow as a 3D mesh rather than a flat quad. The physics engine calculates vertex offsets using soft-body dynamics, and a shader projects the original window texture onto the deformed mesh.

### 6. Intelligent Selection of Distorted Text
Even on a warped window, you can select text normally. Selecting with a cursor follows the curved line on the screen instead of jumping across rows.

*Technical detail:* the application renders into a hidden flat buffer. The compositor does inverse mapping: takes screen coordinates of the cursor, projects them back through the warped UV mesh, and translates them into original flat coordinates.

### 7. Slicing Windows (Slicing)
A "knife" tool cuts a single application (e.g., Telegram) into pieces — the contact list and the chat area become two independent physical windows, while the app continues running as a single process.

*Technical detail:* uses the `wp_viewporter` protocol. The app provides a single image buffer, and the compositor creates two physical bodies, cropping the texture on each. Clicks are translated back with a coordinate offset.

---

## Current Implementation vs. Final Concept

| Feature | Status |
|---------|--------|
| Wayland Compositor (compositor, shm, xdg_shell, seat, output) | ✅ Implemented |
| Dual Backends: Winit (nested) + DRM/KMS from TTY | ✅ Implemented |
| Tiling Manager (BSP/Dwindle tree) | ✅ Implemented (no physics yet — standard strict tiling) |
| Hotkeys, focus, mouse resize of tiles | ✅ Implemented |
| VT Switching (`Ctrl+Alt+F1..F12`) | ✅ Implemented |
| On-demand Rendering (damage-gating) | ✅ Implemented |
| Software Cursor in DRM/KMS mode | ✅ Implemented |
| Physics engine rapier2d / gravity (Phase 1) | ✅ `Super+G` toggle, infinite canvas, camera, window drag |
| Advanced physics (Phase 1.1) | ✅ accurate colliders (size sync), free rotation, collisions while spinning, visual floor |
| Window rotation (Phase 1.2) | ✅ GLES-rotated draw, rotated hit-test, hold `Super+A`/`Super+D` (stiff spin → lock angle → free again) |
| DMA-BUF (zero-copy) | ✅ DmabufState / client buffer import |
| wlr-screencopy / wlr-output-management | ✅ basic globals (screen capture and output management) |
| XDG Decoration Support | ✅ clients disable their CSD (titlebars/buttons) for a clean look |
| Drawing physical lines (steel/glue/trampoline/rope) | 📋 Concept |
| Session persistence (serde → JSON/TOML) | 📋 Concept |
| Window joining (Weld/Spring joints) | 📋 Concept |
| Window deformation (soft bodies, mesh) | 📋 Concept |
| Inverse projection for distorted text | 📋 Concept |
| Window slicing (`wp_viewporter`) | 📋 Concept |

---

## Project Structure (Current Implementation)

```
src/
├── main.rs            — Entry point; selects backend (Winit / DRM), runs event loop,
│                        sets up Wayland socket and Output.
├── state.rs           — Global state (AppState), Smithay traits, layout,
│                        physics step, spin hold/release, rotated hit-test.
├── backend_drm.rs     — Native DRM/KMS backend: libseat, GPU, GBM/EGL, libinput,
│                        udev, visual floor, software cursor, screencopy frames.
├── input.rs           — Input (keyboard/mouse), hotkeys, drag, Super+A/D spin.
├── tiling.rs          — BSP/Dwindle tile tree (TileNode).
├── physics.rs         — rapier2d wrapper: world, gravity, floor, window bodies, ω.
├── render.rs          — CustomRenderElements + PhysicsElement (GLES window rotation).
├── screencopy.rs      — wlr-screencopy-unstable-v1 (screen capture).
└── output_manager.rs  — wlr-output-management-unstable-v1.
sandboxWM_concept.md     — Final project vision (concept in Russian).
sandboxWM_concept_EN.md  — Final project vision (concept in English).
```

---

## Architecture Details

### Application State (`AppState`)
The central struct in `state.rs` manages Smithay states (Compositor, Shm, XdgShell, Output, Seat, XdgDecoration, Dmabuf, screencopy, output-management), `Space<Window>`, BSP tile tree, the physics world and window bodies, spin hold state (`physics_spin_*`), TTY session, pointer coordinates, and the `needs_render` flag for damage-gated rendering.

### Tiling Tree (`TileNode`)
```
TileNode::Leaf(Window)
TileNode::Split { dir, ratio, left, right }
```
Split direction is chosen automatically based on aspect ratio; deleting a leaf collapses the parent `Split`.

### Controls
| Key Combination | Action |
|-----------------|--------|
| `Super + Enter` | Spawn terminal `kitty` |
| `Super + Q` / `Esc` | Quit WM |
| `Super + W` | Close active window |
| `Super + G` | Toggle mode: tiling ↔ physics |
| `Super + A` / `Super + D` *(physics, hold)* | Spin focused window left / right; on release the angle stays, then the body is free for physics again |
| `Super + ← / → / ↑ / ↓` | Tiling: switch focus · Physics: move camera |
| `Super + LMB + drag` | Tiling: resize tiling layout · Physics: drag window body |
| `LMB on window` | Focus window and interact with UI |
| `Ctrl + Alt + F1..F12` | Switch VT (TTY/DRM mode only) |

---

## Build and Run

```bash
cargo build --release
```

The mode is determined automatically using the environment variables `WAYLAND_DISPLAY` / `DISPLAY`:

```bash
# Nested mode (within graphical session):
cargo run --release

# Native mode (from TTY, requires libseat, libdrm, libgbm, EGL driver):
cargo run --release
```
