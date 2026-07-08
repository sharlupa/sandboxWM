//! Shared render-element enum.
//!
//! Combines the normal surface elements produced by [`Space`] with a solid-color
//! element used to draw the software cursor in the DRM/KMS backend (which has no
//! hardware cursor plane wired up in Smithay 0.7).
//!
//! Specialised to `GlesRenderer`, the only renderer used by both backends.

use smithay::backend::renderer::{
    element::{
        render_elements,
        solid::SolidColorRenderElement,
        surface::WaylandSurfaceRenderElement,
    },
    gles::GlesRenderer,
};
use smithay::desktop::space::SpaceRenderElements;

render_elements! {
    pub CustomRenderElements<=GlesRenderer>;
    Space=SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor=SolidColorRenderElement,
}
