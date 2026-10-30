//! Курсор мыши для DRM/KMS-бэкенда.
//!
//! Картинку выбирает клиент через `CursorImageStatus`:
//! - `Hidden` — ничего не рисуем;
//! - `Surface` — дерево клиентской cursor-поверхности, сдвинутое на hotspot;
//! - `Named` — иконка из xcursor-темы (`XCURSOR_THEME` / `XCURSOR_SIZE`), а если
//!   темы или иконки нет — встроенная процедурная стрелка.
//!
//! Все элементы помечены `Kind::Cursor`: DrmCompositor кладёт такой элемент на
//! аппаратную cursor-плоскость, если это разрешено FrameFlags и он влезает
//! в `drm.cursor_size()`. Иначе элемент просто композитится поверх окон.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::surface::{
    WaylandSurfaceRenderElement, render_elements_from_surface_tree,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Transform};
use smithay::wayland::compositor::{SurfaceAttributes, with_states};

use crate::render::CustomRenderElements;

/// Номинальный размер курсора, если `XCURSOR_SIZE` не задан.
const DEFAULT_CURSOR_SIZE: u32 = 24;

/// Базовая сетка встроенной стрелки (при номинальном размере 24).
const ARROW_GRID: (u32, u32) = (12, 19);

/// Контур встроенной стрелки в клетках сетки `ARROW_GRID` (при k=1 даёт
/// классическую стрелку 12×19). Остриё чуть выше (0, 0), чтобы центр пикселя
/// (0, 0) гарантированно попал внутрь, а правый край шёл ровной лесенкой под
/// 45°; остальные вершины подобраны так, чтобы центры пикселей не ложились на
/// рёбра. Обход: правый край → крыло → хвост → выемка → левый край.
const ARROW_POLYGON: [(f32, f32); 7] = [
    (0.0, -0.5),
    (12.5, 12.0),
    (7.7, 12.0),
    (10.7, 18.0),
    (7.15, 19.0),
    (4.3, 13.3),
    (0.0, 17.6),
];

/// Один кадр курсора: готовый к импорту буфер и горячая точка.
struct CursorFrame {
    buffer: MemoryRenderBuffer,
    /// Горячая точка в пикселях буфера.
    hotspot: (i32, i32),
}

/// Изображение курсора: один или несколько (анимация) кадров одного размера.
struct CursorImage {
    frames: Vec<CursorFrame>,
    /// Длительность каждого кадра в мс (для статичных курсоров не важна).
    delays: Vec<u32>,
    /// Целочисленный масштаб буфера: пиксели буфера / логические пиксели.
    buffer_scale: i32,
}

impl CursorImage {
    fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    fn frame_at(&self, millis: u64) -> usize {
        frame_index(&self.delays, millis).min(self.frames.len().saturating_sub(1))
    }
}

/// Что было нарисовано в прошлый раз — чтобы понять, нужна ли перерисовка.
struct Drawn {
    status: CursorImageStatus,
    location: Point<f64, Logical>,
    image: Option<Rc<CursorImage>>,
    frame: usize,
}

/// Строит render-элементы курсора и кэширует изображения из xcursor-темы.
///
/// Держите один экземпляр на всё время работы рендера (как и сам рендерер):
/// текстуры кэшируются внутри `MemoryRenderBuffer`.
pub struct CursorRenderer {
    theme_name: String,
    size: u32,
    /// Тема грузится лениво — при первом `Named`-курсоре.
    theme: Option<xcursor::CursorTheme>,
    /// (иконка, целый масштаб) → изображение; `None` — в теме такой нет.
    named: HashMap<(CursorIcon, u32), Option<Rc<CursorImage>>>,
    /// Процедурная стрелка по целому масштабу.
    fallback: HashMap<u32, Rc<CursorImage>>,
    /// Точка отсчёта времени для анимированных курсоров.
    start: Instant,
    last: Option<Drawn>,
    import_error_reported: bool,
}

impl Default for CursorRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorRenderer {
    /// Читает `XCURSOR_THEME` (по умолчанию "default") и `XCURSOR_SIZE`
    /// (по умолчанию 24). Сама тема загружается при первом использовании.
    pub fn new() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "default".to_string());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_CURSOR_SIZE)
            .clamp(8, 256);
        Self {
            theme_name,
            size,
            theme: None,
            named: HashMap::new(),
            fallback: HashMap::new(),
            start: Instant::now(),
            last: None,
            import_error_reported: false,
        }
    }

    /// Элементы курсора для указателя в `location` (логические координаты
    /// выхода) при масштабе `scale`. Порядок — front-to-back, как ждёт
    /// `render_frame`: вставляйте их в НАЧАЛО общего вектора элементов.
    ///
    /// Если клиентская cursor-поверхность умерла, `status` сбрасывается на
    /// стрелку по умолчанию.
    pub fn elements(
        &mut self,
        renderer: &mut GlesRenderer,
        status: &mut CursorImageStatus,
        location: Point<f64, Logical>,
        scale: f64,
    ) -> Vec<CustomRenderElements> {
        if let CursorImageStatus::Surface(surface) = status
            && !surface.alive()
        {
            *status = CursorImageStatus::default_named();
        }

        let mut image_drawn = None;
        let mut frame_drawn = 0;
        let elements = match status {
            CursorImageStatus::Hidden => Vec::new(),
            CursorImageStatus::Surface(surface) => {
                let hotspot = apply_hotspot_delta(surface).to_f64();
                let pos = (location - hotspot).to_physical(scale).to_i32_round();
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    surface,
                    pos,
                    scale,
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(CustomRenderElements::Surface)
                .collect()
            }
            CursorImageStatus::Named(icon) => {
                let image = self.image_for(*icon, scale);
                let frame = image.frame_at(self.millis());
                let element = image_element(renderer, &image, frame, location, scale);
                image_drawn = Some(image);
                frame_drawn = frame;
                match element {
                    Ok(element) => vec![CustomRenderElements::CursorImage(element)],
                    Err(e) => {
                        if !self.import_error_reported {
                            eprintln!("[cursor] не удалось загрузить текстуру курсора: {e:?}");
                            self.import_error_reported = true;
                        }
                        Vec::new()
                    }
                }
            }
        };

        self.last = Some(Drawn {
            status: status.clone(),
            location,
            image: image_drawn,
            frame: frame_drawn,
        });
        elements
    }

    /// Нужно ли перерисовать кадр ради курсора, даже если больше ничего не
    /// менялось: указатель сдвинулся, клиент сменил курсор, cursor-поверхность
    /// умерла или анимированный курсор перешёл на следующий кадр.
    pub fn needs_redraw(&self, status: &CursorImageStatus, location: Point<f64, Logical>) -> bool {
        let Some(last) = &self.last else {
            return true;
        };
        if last.status != *status || last.location != location {
            return true;
        }
        if let CursorImageStatus::Surface(surface) = status
            && !surface.alive()
        {
            return true;
        }
        match &last.image {
            Some(image) if image.is_animated() => image.frame_at(self.millis()) != last.frame,
            _ => false,
        }
    }

    fn millis(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Изображение для именованного курсора: иконка темы → стрелка темы →
    /// встроенная стрелка.
    fn image_for(&mut self, icon: CursorIcon, scale: f64) -> Rc<CursorImage> {
        let int_scale = integer_scale(scale);
        if let Some(image) = self.themed(icon, int_scale) {
            return image;
        }
        if icon != CursorIcon::Default
            && let Some(image) = self.themed(CursorIcon::Default, int_scale)
        {
            return image;
        }
        let size = self.size;
        self.fallback
            .entry(int_scale)
            .or_insert_with(|| Rc::new(arrow_image(size, int_scale)))
            .clone()
    }

    fn themed(&mut self, icon: CursorIcon, int_scale: u32) -> Option<Rc<CursorImage>> {
        if let Some(cached) = self.named.get(&(icon, int_scale)) {
            return cached.clone();
        }
        let theme_name = &self.theme_name;
        let theme = self
            .theme
            .get_or_insert_with(|| xcursor::CursorTheme::load(theme_name));
        let loaded = load_xcursor(theme, icon)
            .and_then(|images| image_from_xcursor(&images, self.size, int_scale))
            .map(Rc::new);
        self.named.insert((icon, int_scale), loaded.clone());
        loaded
    }
}

/// Отправляет frame callbacks клиентской cursor-поверхности (иначе
/// анимированные курсоры клиентов «замирают»). Вызывать после успешного
/// рендера кадра, рядом с `send_frame` для окон.
pub fn send_frames(status: &CursorImageStatus, output: &Output, time: impl Into<Duration>) {
    if let CursorImageStatus::Surface(surface) = status
        && surface.alive()
    {
        smithay::desktop::utils::send_frames_surface_tree(
            surface,
            output,
            time,
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }
}

/// Возвращает hotspot cursor-поверхности, предварительно учтя
/// `wl_surface.offset` / `attach(dx, dy)`: по протоколу wl_pointer hotspot
/// уменьшается на (dx, dy).
///
/// Надёжнее всего вызывать это из `CompositorHandler::commit` для каждого
/// коммита (для не-курсорных поверхностей функция ничего не делает): если
/// между двумя кадрами пришло несколько коммитов со смещением, рендер увидит
/// только последнее. `CursorRenderer` вызывает её и сам — как запасной путь.
pub fn apply_hotspot_delta(surface: &WlSurface) -> Point<i32, Logical> {
    with_states(surface, |states| {
        let Some(attrs) = states.data_map.get::<CursorImageSurfaceData>() else {
            return Point::default();
        };
        let mut attrs = attrs.lock().unwrap_or_else(PoisonError::into_inner);
        let delta = states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer_delta
            .take();
        if let Some(delta) = delta {
            attrs.hotspot -= delta;
        }
        attrs.hotspot
    })
}

/// Элемент для кадра `frame` изображения `image`, горячей точкой в `location`.
fn image_element(
    renderer: &mut GlesRenderer,
    image: &CursorImage,
    frame: usize,
    location: Point<f64, Logical>,
    scale: f64,
) -> Result<
    MemoryRenderBufferRenderElement<GlesRenderer>,
    smithay::backend::renderer::gles::GlesError,
> {
    let frame = &image.frames[frame.min(image.frames.len() - 1)];
    let buffer_scale = image.buffer_scale as f64;
    let hotspot: Point<f64, Logical> = (
        frame.hotspot.0 as f64 / buffer_scale,
        frame.hotspot.1 as f64 / buffer_scale,
    )
        .into();
    // Целые физические координаты: курсор не размывается, а DrmCompositor
    // может скопировать буфер на cursor-плоскость один в один.
    let pos = (location - hotspot)
        .to_physical(scale)
        .to_i32_round::<i32>()
        .to_f64();
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        pos,
        &frame.buffer,
        None,
        None,
        None,
        Kind::Cursor,
    )
}

/// Целый масштаб, под который грузятся картинки (дробный округляем вверх —
/// лучше чуть уменьшить картинку, чем растянуть).
fn integer_scale(scale: f64) -> u32 {
    if scale.is_finite() && scale > 1.0 {
        scale.ceil().min(8.0) as u32
    } else {
        1
    }
}

/// Индекс кадра анимации для момента `millis` (время зациклено по сумме задержек).
fn frame_index(delays: &[u32], millis: u64) -> usize {
    let total: u64 = delays.iter().map(|&d| d as u64).sum();
    if delays.len() <= 1 || total == 0 {
        return 0;
    }
    let mut t = millis % total;
    for (i, &delay) in delays.iter().enumerate() {
        if t < delay as u64 {
            return i;
        }
        t -= delay as u64;
    }
    delays.len() - 1
}

/// Номинальный размер из файла, ближайший к желаемому.
fn nearest_nominal_size(sizes: impl IntoIterator<Item = u32>, target: u32) -> Option<u32> {
    sizes
        .into_iter()
        .min_by_key(|&size| (size as i64 - target as i64).abs())
}

/// Ищет иконку (по основному и альтернативным именам) и разбирает xcursor-файл.
fn load_xcursor(
    theme: &xcursor::CursorTheme,
    icon: CursorIcon,
) -> Option<Vec<xcursor::parser::Image>> {
    let path = std::iter::once(icon.name())
        .chain(icon.alt_names().iter().copied())
        .find_map(|name| theme.load_icon(name))?;
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("[cursor] не удалось прочитать {path:?}: {e}");
            return None;
        }
    };
    let images = xcursor::parser::parse_xcursor(&data).filter(|images| !images.is_empty());
    if images.is_none() {
        eprintln!("[cursor] повреждённый xcursor-файл {path:?}");
    }
    images
}

/// Собирает изображение из кадров xcursor-файла ближайшего размера.
fn image_from_xcursor(
    images: &[xcursor::parser::Image],
    base_size: u32,
    int_scale: u32,
) -> Option<CursorImage> {
    let nominal = nearest_nominal_size(images.iter().map(|i| i.size), base_size * int_scale)?;
    let buffer_scale = ((nominal as f64 / base_size as f64).round() as i32).max(1);

    let mut frames = Vec::new();
    let mut delays = Vec::new();
    for img in images.iter().filter(|img| img.size == nominal) {
        let expected_len = img.width as usize * img.height as usize * 4;
        if img.pixels_rgba.len() != expected_len {
            continue;
        }
        // Пиксели xcursor — ARGB32 little-endian в порядке файла (B,G,R,A),
        // premultiplied: ровно Argb8888, который DrmCompositor умеет класть на
        // cursor-плоскость без перерисовки.
        frames.push(CursorFrame {
            buffer: MemoryRenderBuffer::from_slice(
                &img.pixels_rgba,
                Fourcc::Argb8888,
                (img.width as i32, img.height as i32),
                buffer_scale,
                Transform::Normal,
                None,
            ),
            hotspot: (img.xhot as i32, img.yhot as i32),
        });
        delays.push(img.delay);
    }
    if frames.is_empty() {
        return None;
    }
    Some(CursorImage {
        frames,
        delays,
        buffer_scale,
    })
}

/// Встроенная стрелка для курсора номинального размера `base_size`.
fn arrow_image(base_size: u32, int_scale: u32) -> CursorImage {
    let (pixels, w, h) = arrow_pixels(arrow_factor(base_size, int_scale));
    CursorImage {
        frames: vec![CursorFrame {
            buffer: MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Argb8888,
                (w, h),
                int_scale as i32,
                Transform::Normal,
                None,
            ),
            hotspot: (0, 0),
        }],
        delays: vec![0],
        buffer_scale: int_scale as i32,
    }
}

/// Во сколько раз увеличить сетку 12×19 (она соответствует размеру 24):
/// только целым множителем, чтобы не мылить картинку.
fn arrow_factor(base_size: u32, int_scale: u32) -> u32 {
    ((base_size * int_scale) as f64 / DEFAULT_CURSOR_SIZE as f64)
        .round()
        .max(1.0) as u32
}

/// Растеризует стрелку с увеличением `k`: белая заливка, чёрный контур
/// толщиной `k` пикселей, прозрачный фон. Формат — Argb8888 little-endian
/// (в памяти B,G,R,A), горячая точка — пиксель (0, 0).
fn arrow_pixels(k: u32) -> (Vec<u8>, i32, i32) {
    let k = k.clamp(1, 16);
    let (w, h) = ((ARROW_GRID.0 * k) as i64, (ARROW_GRID.1 * k) as i64);
    let inside: Vec<bool> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| {
            let px = (x as f32 + 0.5) / k as f32;
            let py = (y as f32 + 0.5) / k as f32;
            point_in_polygon(px, py, &ARROW_POLYGON)
        })
        .collect();
    let is_inside =
        |x: i64, y: i64| x >= 0 && y >= 0 && x < w && y < h && inside[(y * w + x) as usize];

    let t = k as i64;
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
        let (x, y) = (i as i64 % w, i as i64 / w);
        if !is_inside(x, y) {
            continue;
        }
        // Контур — пиксели заливки, у которых в пределах `t` по горизонтали
        // или вертикали есть пустота (4-соседство даёт ровную диагональ).
        let edge = (1..=t).any(|d| {
            !is_inside(x - d, y)
                || !is_inside(x + d, y)
                || !is_inside(x, y - d)
                || !is_inside(x, y + d)
        });
        let c = if edge { 0x00 } else { 0xFF };
        px.copy_from_slice(&[c, c, c, 0xFF]);
    }
    (pixels, w as i32, h as i32)
}

/// Чётно-нечётный тест попадания точки в многоугольник.
fn point_in_polygon(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ascii(pixels: &[u8], w: i32) -> String {
        pixels
            .chunks_exact(4)
            .collect::<Vec<_>>()
            .chunks(w as usize)
            .map(|row| {
                row.iter()
                    .map(|px| match (px[3], px[0]) {
                        (0, _) => ' ',
                        (_, 0) => 'X',
                        _ => '.',
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn arrow_shape() {
        let (pixels, w, h) = arrow_pixels(1);
        println!("{}", ascii(&pixels, w));
        assert_eq!((w, h), (12, 19));
        assert_eq!(pixels.len(), (w * h * 4) as usize);

        let px = |x: i32, y: i32| {
            let i = ((y * w + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // Горячая точка — непрозрачный пиксель контура.
        assert_eq!(px(0, 0), [0, 0, 0, 0xFF]);
        // Левый край — контур, внутри — белая заливка, справа сверху — пусто.
        assert_eq!(px(0, 8), [0, 0, 0, 0xFF]);
        assert_eq!(px(2, 8), [0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(px(11, 0)[3], 0);
        // Каждая строка тела стрелки начинается с контура и на нём же кончается.
        for y in 0..12 {
            let row: Vec<_> = (0..w).map(|x| px(x, y)).collect();
            let first = row.iter().position(|p| p[3] != 0).unwrap();
            let last = row.iter().rposition(|p| p[3] != 0).unwrap();
            assert_eq!(first, 0, "строка {y}");
            assert_eq!(row[last], [0, 0, 0, 0xFF], "строка {y}");
            assert_eq!(last as i32, y, "правый край идёт под 45°, строка {y}");
        }
        // Есть белая заливка и в хвосте.
        assert!((13..18).any(|y| (0..w).any(|x| px(x, y) == [0xFF; 4])));
        // Только чистые цвета: прозрачный, чёрный или белый.
        for p in pixels.chunks_exact(4) {
            assert!(p == [0, 0, 0, 0] || p == [0, 0, 0, 0xFF] || p == [0xFF; 4]);
        }
    }

    #[test]
    fn arrow_scales_with_integer_factor() {
        let (small, w1, h1) = arrow_pixels(1);
        let (big, w2, h2) = arrow_pixels(2);
        println!("{}", ascii(&big, w2));
        assert_eq!((w2, h2), (w1 * 2, h1 * 2));
        let opaque = |p: &[u8]| p.chunks_exact(4).filter(|p| p[3] != 0).count();
        // Площадь растёт примерно вчетверо.
        let (a1, a2) = (opaque(&small), opaque(&big));
        assert!(a2 > a1 * 3 && a2 < a1 * 5, "{a1} → {a2}");
        // Контур при k=2 толщиной два пикселя.
        let i = |x: i32, y: i32| ((y * w2 + x) * 4) as usize;
        assert_eq!(big[i(0, 16)], 0);
        assert_eq!(big[i(1, 16)], 0);
        assert_eq!(big[i(4, 16)], 0xFF);
    }

    #[test]
    fn arrow_size_follows_cursor_size() {
        assert_eq!(arrow_factor(24, 1), 1);
        assert_eq!(arrow_factor(16, 1), 1);
        assert_eq!(arrow_factor(48, 1), 2);
        assert_eq!(arrow_factor(24, 2), 2);
        assert_eq!(arrow_factor(32, 3), 4);
        let image = arrow_image(24, 2);
        assert_eq!(image.buffer_scale, 2);
        assert_eq!(image.frames.len(), 1);
        assert_eq!(image.frames[0].hotspot, (0, 0));
    }

    #[test]
    fn animation_frame_selection() {
        assert_eq!(frame_index(&[], 1000), 0);
        assert_eq!(frame_index(&[50], 1000), 0);
        assert_eq!(frame_index(&[0, 0, 0], 1000), 0);
        let delays = [100, 50, 200];
        assert_eq!(frame_index(&delays, 0), 0);
        assert_eq!(frame_index(&delays, 99), 0);
        assert_eq!(frame_index(&delays, 100), 1);
        assert_eq!(frame_index(&delays, 149), 1);
        assert_eq!(frame_index(&delays, 150), 2);
        assert_eq!(frame_index(&delays, 349), 2);
        // Зацикливание.
        assert_eq!(frame_index(&delays, 350), 0);
        assert_eq!(frame_index(&delays, 350 * 1000 + 120), 1);
        // Кадры с нулевой задержкой пропускаются.
        assert_eq!(frame_index(&[0, 30, 0, 30], 10), 1);
        assert_eq!(frame_index(&[0, 30, 0, 30], 40), 3);
    }

    #[test]
    fn nominal_size_selection() {
        assert_eq!(nearest_nominal_size([], 24), None);
        assert_eq!(nearest_nominal_size([24, 32, 48], 24), Some(24));
        assert_eq!(nearest_nominal_size([32, 48, 64], 24), Some(32));
        assert_eq!(nearest_nominal_size([24, 32, 48, 64, 96], 48), Some(48));
        assert_eq!(nearest_nominal_size([24, 24, 24], 48), Some(24));
    }

    #[test]
    fn integer_scale_rounds_up() {
        assert_eq!(integer_scale(1.0), 1);
        assert_eq!(integer_scale(0.5), 1);
        assert_eq!(integer_scale(1.25), 2);
        assert_eq!(integer_scale(2.0), 2);
        assert_eq!(integer_scale(f64::NAN), 1);
    }

    #[test]
    fn xcursor_frames_of_nearest_size() {
        let img = |size: u32, w: u32, delay: u32, fill: u8| xcursor::parser::Image {
            size,
            width: w,
            height: w,
            xhot: 1,
            yhot: 2,
            delay,
            pixels_rgba: vec![fill; (w * w * 4) as usize],
            pixels_argb: vec![fill; (w * w * 4) as usize],
        };
        let images = vec![
            img(24, 24, 30, 1),
            img(24, 24, 40, 2),
            img(48, 48, 30, 3),
            img(48, 48, 40, 4),
        ];
        let small = image_from_xcursor(&images, 24, 1).unwrap();
        assert_eq!(small.frames.len(), 2);
        assert_eq!(small.delays, vec![30, 40]);
        assert_eq!(small.buffer_scale, 1);
        assert!(small.is_animated());
        assert_eq!(small.frame_at(35), 1);

        let big = image_from_xcursor(&images, 24, 2).unwrap();
        assert_eq!(big.buffer_scale, 2);
        assert_eq!(big.delays, vec![30, 40]);
        assert_eq!(big.frames[0].hotspot, (1, 2));

        // Под масштаб 2 нет картинки 48 — берём 24 c buffer_scale 1 (растянется).
        let only_small = image_from_xcursor(&images[..2], 24, 2).unwrap();
        assert_eq!(only_small.buffer_scale, 1);

        // Битый кадр (не тот размер пикселей) отбрасывается.
        let mut broken = img(24, 24, 0, 0);
        broken.pixels_rgba.truncate(10);
        assert!(image_from_xcursor(&[broken], 24, 1).is_none());
    }

    #[test]
    fn named_cursor_elements_with_gles() {
        use smithay::backend::renderer::element::Element;
        use smithay::utils::Scale;

        let Some(mut renderer) = crate::screencopy::headless_renderer() else {
            eprintln!("нет headless EGL — тест пропущен");
            return;
        };
        let mut cursor = CursorRenderer::new();
        let pointer: Point<f64, Logical> = (20.3, 10.6).into();

        // Hidden — пусто; смена статуса требует перерисовки.
        let mut status = CursorImageStatus::Hidden;
        assert!(cursor.needs_redraw(&status, pointer));
        assert!(
            cursor
                .elements(&mut renderer, &mut status, pointer, 1.0)
                .is_empty()
        );
        assert!(!cursor.needs_redraw(&status, pointer));
        // Сдвиг указателя — тоже повод перерисовать.
        assert!(cursor.needs_redraw(&status, (21.0, 10.6).into()));
        status = CursorImageStatus::default_named();
        assert!(cursor.needs_redraw(&status, pointer));

        // Стрелка темы (или встроенная): один элемент Kind::Cursor, горячая
        // точка — в позиции указателя (с округлением до пикселя).
        let elements = cursor.elements(&mut renderer, &mut status, pointer, 1.0);
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].kind(), Kind::Cursor);
        assert!(matches!(elements[0], CustomRenderElements::CursorImage(_)));
        let image = cursor.image_for(CursorIcon::Default, 1.0);
        let (hx, hy) = image.frames[0].hotspot;
        let bs = image.buffer_scale as f64;
        let geo = elements[0].geometry(Scale::from(1.0));
        assert_eq!(
            (geo.loc.x, geo.loc.y),
            (
                (20.3 - hx as f64 / bs).round() as i32,
                (10.6 - hy as f64 / bs).round() as i32
            )
        );
        assert!(!cursor.needs_redraw(&status, pointer));

        // Иконка, которой нет в теме, даёт ту же стрелку, а не пустоту.
        let mut odd = CursorImageStatus::Named(CursorIcon::ZoomOut);
        assert_eq!(
            cursor.elements(&mut renderer, &mut odd, pointer, 1.0).len(),
            1
        );
    }

    #[test]
    fn builtin_arrow_draws_with_gles() {
        let Some(mut renderer) = crate::screencopy::headless_renderer() else {
            eprintln!("нет headless EGL — тест пропущен");
            return;
        };
        let image = arrow_image(24, 1);
        let element = image_element(&mut renderer, &image, 0, (5.0, 3.0).into(), 1.0).unwrap();
        let pixels = crate::screencopy::render_for_tests(
            &mut renderer,
            (32, 32).into(),
            &[CustomRenderElements::CursorImage(element)],
            [0.0, 0.0, 1.0, 1.0],
        );
        let px = |x: usize, y: usize| {
            let i = (y * 32 + x) * 4;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // Горячая точка стрелки (0, 0) — в (5, 3): чёрный контур.
        assert_eq!(px(5, 3), [0, 0, 0, 255]);
        // Заливка — белая (Argb8888 из памяти дошёл до GL без перестановки каналов).
        assert_eq!(px(5 + 2, 3 + 8), [255, 255, 255, 255]);
        // Прозрачные пиксели стрелки и всё вокруг — фон.
        assert_eq!(px(5 + 11, 3), [0, 0, 255, 255]);
        assert_eq!(px(4, 3), [0, 0, 255, 255]);
        assert_eq!(px(5, 3 + 19), [0, 0, 255, 255]);
    }

    #[test]
    fn animated_theme_cursor_requests_redraw() {
        let Some(mut renderer) = crate::screencopy::headless_renderer() else {
            eprintln!("нет headless EGL — тест пропущен");
            return;
        };
        let mut cursor = CursorRenderer::new();
        let image = match cursor.themed(CursorIcon::Wait, 1) {
            Some(image) if image.is_animated() => image,
            _ => {
                eprintln!("в теме нет анимированного курсора — тест пропущен");
                return;
            }
        };
        let mut status = CursorImageStatus::Named(CursorIcon::Wait);
        cursor.elements(&mut renderer, &mut status, (10.0, 10.0).into(), 1.0);
        let frame = cursor.last.as_ref().map(|d| d.frame).unwrap_or_default();
        // Сдвигаем часы анимации на длительность текущего кадра — пора
        // показывать следующий, значит нужна перерисовка.
        cursor.start -= Duration::from_millis(image.delays[frame].max(1) as u64);
        assert!(cursor.needs_redraw(&status, (10.0, 10.0).into()));
        cursor.elements(&mut renderer, &mut status, (10.0, 10.0).into(), 1.0);
        assert_ne!(cursor.last.as_ref().map(|d| d.frame), Some(frame));
    }

    #[test]
    fn system_theme_loads_if_installed() {
        // Зависит от системы: на машинах без xcursor-темы просто проверяем,
        // что поиск не паникует.
        let theme = xcursor::CursorTheme::load("default");
        if let Some(images) = load_xcursor(&theme, CursorIcon::Default) {
            let image = image_from_xcursor(&images, 24, 1).expect("кадры темы");
            assert!(!image.frames.is_empty());
        }
    }
}
