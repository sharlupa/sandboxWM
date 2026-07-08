use smithay::backend::renderer::{
    element::{solid::SolidColorRenderElement, surface::WaylandSurfaceRenderElement},
    gles::GlesRenderer, ImportAll, ImportMem, ImportDmaWl, ImportMemWl, ImportEgl,
};
use smithay::desktop::space::SpaceRenderElements;

smithay::backend::renderer::element::render_elements! {
    pub CustomRenderElements<R> where R: ImportAll + ImportMem + ImportDmaWl + ImportMemWl + ImportEgl;
    Space=SpaceRenderElements<R, WaylandSurfaceRenderElement<R>>,
    Cursor=SolidColorRenderElement,
}
