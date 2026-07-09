# sandboxWM Concept

Project Name: sandboxWM

This is a unique window manager for Wayland that combines infinite tiling with a fully integrated 2D physics engine. It turns the desktop into an interactive sandbox where windows behave like real physical objects.

Developed with AI: This project's concept, architecture, and current codebase are developed with the assistance of AI coding tools.

## Technical Stack:

- **Programming Language:** Rust
- **Wayland Compositor Library:** smithay
- **2D Physics Engine:** rapier2d
- **Rendering:** wgpu / glow

---

## 1. Infinite Tiling and Gravity

### Concept & User Experience:
The desktop is not a bounded rectangle but an infinite space featuring gravity and a physical floor. When you open a new program, its window falls from the top, landing on the floor or stacking on top of other windows. Windows have weight, inertia, and can rest on each other.

The system allows switching gravity modes on the fly:
- **Mode 1 (Standard):** Gravity is enabled globally.
- **Mode 2 (Tiling alignment):** Gravity is disabled, and windows smoothly align into a strict tiling layout.
- **Mode 3 (Individual gravity lock):** Gravity is disabled for one specific window, allowing it to float in place while other windows continue to fall.
- **Mode 4 (Weightlessness):** Gravity is disabled entirely for all windows, introducing "zero gravity".

### Technical Implementation:
Each window is registered as a rigid body (`RigidBody`) in the `rapier2d` physics engine. Gravity is applied via a global force vector in the physical world. Transitioning to standard tiling is achieved by pausing the physics simulation and recalculating window coordinates along a strict grid. Floating a specific window is implemented by changing its body type from `Dynamic` (subject to forces) to `Kinematic` or `Fixed` (static body).

---

## 2. Interactive Drawing (Physical Lines)

### Concept & User Experience:
You can draw lines with the cursor directly onto the desktop. These lines act as physical obstacles that windows collide with. Lines can be configured with different materials:
- **Steel (Floor):** A standard rigid line. Windows can be stacked on top of it like a shelf.
- **Glue:** A sticky line. Windows instantly stick to it upon contact. Detaching them requires pulling them away with mouse drag using high force.
- **Trampoline:** An elastic line. Windows bounce off it upon impact.
- **Rope:** A dynamic line that is subject to gravity. It swings and falls, allowing windows to be hung from it.

*(Window parameters and collision responses can be tweaked in the settings).*

### Technical Implementation:
The compositor records the sequence of coordinates from cursor movements during drawing and constructs a chain of colliders (`Collider`) in the physics engine. Material properties are configured via parameters: `friction` (high friction creates the glue effect) and `restitution` (bounciness creates the trampoline effect). Ropes are simulated by connecting multiple short rigid bodies using ball joints (`BallJoint`).

---

## 3. Session Persistence

### Concept & User Experience:
On logout or system reboot, the entire workspace layout is preserved. Upon launching the WM next time, all drawn shelves, trampolines, ropes, and application windows reappear at their exact coordinates with their original physical properties intact.

### Technical Implementation:
Upon shutdown, the compositor gathers active state data (window coordinates, arrays of line points, and their material types) into a single data structure, serializing it to a JSON or TOML file using the `serde` library. On startup, the compositor deserializes the file, reconstructs the geometries in `rapier2d`, and spawns application windows at their saved coordinates.

---

## 4. Joining Windows (Joints)

### Concept & User Experience:
Windows can be linked together:
- **Welding:** Two windows (e.g., a browser and a terminal) are welded into a single rigid structure. Moving one window moves the other; they rotate and fall together.
- **Spring:** Windows are connected by a spring (visible or invisible). Pulling a window away causes it to spring back to the other window.
- **Rope:** Windows are connected by a physical rope. By default, the rope does not collide with the linked windows themselves.

### Technical Implementation:
This utilizes the joint system built into the physics engine. Welding is mapped to a `WeldJoint`, and the spring is mapped to a `SpringJoint`. The compositor creates a joint constraint between the two bodies, and the physics engine resolves their combined motion.

---

## 5. Full Window Deformation (Soft Bodies)

### Concept & User Experience:
Windows are no longer restricted to rigid rectangles. You can drag a corner to warp it into a triangle, a circle, or squeeze it. Under a hard collision with the floor, a window might temporarily squash like jelly. The text and UI elements inside warp dynamically to match the deformed window shape.

### Technical Implementation:
Windows are rendered using `wgpu` or `glow` as a 3D polygonal mesh instead of a simple flat quad. The physics engine calculates vertex offsets using soft-body dynamics. A vertex/fragment shader then maps the application's flat output texture onto the deformed mesh, producing the visual deformation effect.

*(The softness, elasticity, and jelly-like properties can be adjusted in settings).*

---

## 6. Intelligent Text Selection on Warped Surfaces

### Concept & User Experience:
Even if a window is heavily warped or bent, selecting text remains intuitive. Dragging the mouse cursor along a curved text line selects the letters smoothly without jumping erratically between adjacent lines.

### Technical Implementation:
The underlying application renders its UI into a hidden, flat buffer and remains oblivious to the screen-space distortion. The compositor performs mathematical inverse mapping: it captures the cursor's screen-space coordinates, projects them back through the warped UV coordinates of the mesh, and translates them back to flat buffer space before sending pointer input to the application. The app selects the text in flat coordinates, and the compositor renders the highlighted text onto the deformed mesh.

---

## 7. Window Slicing

### Concept & User Experience:
You can use a "knife" tool to split an application window (e.g., Telegram) into multiple parts. For example, the contact list and the chat view can become two independent physical windows that can be moved to different parts of the screen. The application itself continues to run seamlessly as a single process.

### Technical Implementation:
This feature utilizes the Wayland `wp_viewporter` protocol. The application renders to a single large buffer. The compositor creates two separate physical bodies and applies cropping, displaying distinct portions of the shared buffer on each body. Click coordinates are translated by adding an offset before being passed to the application, ensuring accurate click handling.
