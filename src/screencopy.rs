//! wlr-screencopy-unstable-v1 (v3): захват экрана для grim, OBS (через
//! xdg-desktop-portal-wlr) и т.п.
//!
//! Две половины:
//! - протокол — [`ScreencopyState`]: принимает запросы, объявляет буфер
//!   (wl_shm Argb8888 размером с выход или регион), проверяет присланный
//!   клиентом wl_buffer и держит очередь кадров;
//! - рендер — [`ScreencopyRenderer`]: в цикле рендера выхода рисует сцену в
//!   offscreen-текстуру, читает нужный регион и пишет пиксели в shm-буфер
//!   клиента с учётом его формата и stride (или копирует блитом в его
//!   dmabuf). `copy_with_damage` ждёт, пока в регионе что-то изменится, и перед
//!   `ready` получает события `damage`.
//!
//! xdg-desktop-portal-wlr 0.7 с протоколом v3 не стартует без события
//! `linux_dmabuf`: объявляем dmabuf, если рендерер умеет в него рисовать.
//!
//! `SANDBOXWM_DEBUG_SCREENCOPY=1` печатает жизненный цикл кадров в stderr.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTarget, GlesTexture};
use smithay::backend::renderer::{
    Bind, Blit, Color32F, ExportMem, Offscreen, Texture, TextureFilter,
};
use smithay::output::Output;
use smithay::utils::{Buffer, Clock, Monotonic, Physical, Rectangle, Size, Transform};
use smithay::wayland::shm::{self, BufferData};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::{ClientId, GlobalId},
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
};

use crate::render::CustomRenderElements;

/// shm-формат, который объявляем клиентам, — как wlroots, один: Xrgb8888.
/// К Argb portal-wlr предлагает PipeWire и вариант без альфы, а выбери его
/// потребитель (OBS) — не находит такого shm-формата у нас и не создаёт
/// буферы. Несколько событий `buffer` подряд сбивают wf-recorder (copy дважды).
/// Клиент может прислать и Argb/Abgr/Xbgr — их тоже пишем, альфа всегда 0xFF.
const ADVERTISED_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

/// dmabuf-форматы, которые готовы объявить, по убыванию предпочтения.
const DMABUF_FORMATS: [Fourcc; 2] = [Fourcc::Xrgb8888, Fourcc::Argb8888];

fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os("SANDBOXWM_DEBUG_SCREENCOPY").is_some_and(|v| v != "0"))
}

macro_rules! sc_debug {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[screencopy] {}", format_args!($($arg)*));
        }
    };
}

/// Сколько прямоугольников damage копить на менеджер, прежде чем схлопнуть их
/// в один охватывающий.
const MAX_DAMAGE_RECTS: usize = 32;

/// Регион из `capture_output_region` как его прислал клиент: (x, y, ширина,
/// высота) в логических координатах выхода. Держим сырые числа — `Size` из
/// smithay в debug-сборке паникует на отрицательных размерах.
type RequestedRegion = (i32, i32, i32, i32);

// ---------------------------------------------------------
// Протокол: состояние и очередь кадров
// ---------------------------------------------------------

pub struct ScreencopyState {
    _global: GlobalId,
    /// Кадры, которые клиенты создали и которые ещё не получили ready/failed.
    pub pending_frames: Arc<Mutex<Vec<PendingFrame>>>,
    /// Формат для события `linux_dmabuf`; None — рендерер не рисует в dmabuf.
    dmabuf_format: Option<Fourcc>,
}

/// Буфер клиента из `copy` / `copy_with_damage`.
enum CaptureBuffer {
    Shm(WlBuffer),
    /// Рисуем в него блитом из offscreen-текстуры, без чтения в память.
    Dmabuf(Dmabuf),
}

/// Кадр захвата: ждёт `copy` от клиента, а затем — рендера своего выхода.
pub struct PendingFrame {
    frame: ZwlrScreencopyFrameV1,
    /// Менеджер, через который создан кадр: damage по протоколу считается
    /// «с прошлого copy этого менеджера».
    manager: Weak<ZwlrScreencopyManagerV1>,
    output: Output,
    /// Запрошенный регион (None — весь выход).
    requested_region: Option<RequestedRegion>,
    /// Тот же регион в пикселях выхода — под него объявлен буфер.
    region: Rectangle<i32, Physical>,
    overlay_cursor: bool,
    /// Проверенный буфер из `copy` / `copy_with_damage`.
    buffer: Option<CaptureBuffer>,
    with_damage: bool,
}

impl ScreencopyState {
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: GlobalDispatch<ZwlrScreencopyManagerV1, ()> + 'static,
    {
        let global = display.create_global::<D, ZwlrScreencopyManagerV1, ()>(3, ());
        Self {
            _global: global,
            pending_frames: Arc::new(Mutex::new(Vec::new())),
            dmabuf_format: None,
        }
    }

    /// Включает объявление dmabuf-буферов, если `renderer` умеет рисовать в
    /// один из подходящих форматов. Вызывается бэкендом после создания рендерера.
    /// `SANDBOXWM_SCREENCOPY_DMABUF=0` отключает dmabuf (останется только shm).
    pub fn set_renderer(&mut self, renderer: &GlesRenderer) {
        if std::env::var_os("SANDBOXWM_SCREENCOPY_DMABUF").is_some_and(|v| v == "0") {
            self.dmabuf_format = None;
            println!("[screencopy] dmabuf отключён (SANDBOXWM_SCREENCOPY_DMABUF=0)");
            return;
        }
        let formats = renderer.egl_context().dmabuf_render_formats();
        self.dmabuf_format = DMABUF_FORMATS
            .into_iter()
            .find(|code| formats.iter().any(|f| f.code == *code));
        println!("[screencopy] dmabuf-формат: {:?}", self.dmabuf_format);
    }

    fn frames(&self) -> MutexGuard<'_, Vec<PendingFrame>> {
        self.pending_frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Начинает захват: объявляет параметры буфера и ставит кадр в очередь.
    fn start_capture(
        &self,
        frame: ZwlrScreencopyFrameV1,
        manager: &ZwlrScreencopyManagerV1,
        wl_output: &WlOutput,
        overlay_cursor: bool,
        requested_region: Option<RequestedRegion>,
    ) {
        // wl_output может пережить свой Output (монитор отключили), а регион —
        // оказаться целиком за пределами выхода: захватывать нечего.
        let Some(output) = Output::from_resource(wl_output) else {
            frame.failed();
            return;
        };
        let Some(region) = capture_region(&output, requested_region) else {
            frame.failed();
            return;
        };

        let (width, height) = (region.size.w as u32, region.size.h as u32);
        frame.buffer(ADVERTISED_FORMAT, width, height, width * 4);
        if frame.version() >= 3 {
            // portal-wlr 0.7 без linux_dmabuf в v3 не начинает захват.
            if let Some(format) = self.dmabuf_format {
                frame.linux_dmabuf(format as u32, width, height);
            }
            // После перечисления всех типов буферов клиент ждёт buffer_done.
            // Без него xdg-desktop-portal-wlr зависает и захват не стартует.
            frame.buffer_done();
        }
        sc_debug!(
            "capture v{} {}x{} cursor={} dmabuf={:?}",
            frame.version(),
            width,
            height,
            overlay_cursor,
            self.dmabuf_format
        );

        self.frames().push(PendingFrame {
            frame,
            manager: manager.downgrade(),
            output,
            requested_region,
            region,
            overlay_cursor,
            buffer: None,
            with_damage: false,
        });
    }

    /// `copy` / `copy_with_damage`: проверяем буфер сразу, чтобы неподходящий
    /// получил `failed`, а не висел в очереди.
    fn attach_buffer(&self, frame: &ZwlrScreencopyFrameV1, buffer: WlBuffer, with_damage: bool) {
        let mut frames = self.frames();
        // Кадр уже получил ready/failed — повторный copy просто игнорируем.
        let Some(index) = frames.iter().position(|f| f.frame == *frame) else {
            return;
        };
        if frames[index].buffer.is_some() {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "copy was already requested for this frame",
            );
            frames.remove(index);
            return;
        }

        let size = frames[index].region.size;
        let checked = match shm::with_buffer_contents(&buffer, |_, len, data| {
            check_shm_buffer(&data, len, size)
        }) {
            Ok(Ok(_)) => Ok(CaptureBuffer::Shm(buffer)),
            Ok(Err(reason)) => Err(reason.to_string()),
            Err(_) => match smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                Ok(_) if self.dmabuf_format.is_none() => Err("dmabuf не объявлялся".into()),
                Ok(dmabuf) => check_dmabuf(dmabuf, size).map(CaptureBuffer::Dmabuf),
                Err(_) => Err("буфер не wl_shm и не dmabuf".into()),
            },
        };
        match checked {
            Ok(buffer) => {
                sc_debug!(
                    "copy{} ({})",
                    if with_damage { "_with_damage" } else { "" },
                    if matches!(buffer, CaptureBuffer::Shm(_)) {
                        "shm"
                    } else {
                        "dmabuf"
                    }
                );
                let pending = &mut frames[index];
                pending.buffer = Some(buffer);
                pending.with_damage = with_damage;
            }
            Err(reason) => {
                eprintln!("[screencopy] буфер клиента отклонён: {reason}");
                frames.remove(index).frame.failed();
            }
        }
    }

    fn remove_frame(&self, frame: &ZwlrScreencopyFrameV1) {
        self.frames().retain(|f| f.frame != *frame);
    }
}

// ---------------------------------------------------------
// Manager Implementation
// ---------------------------------------------------------

impl<D> GlobalDispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyManagerV1, ()>
        + AsMut<ScreencopyState>,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<ZwlrScreencopyManagerV1, (), D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyFrameV1, ()>
        + AsMut<ScreencopyState>,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let screencopy_state = state.as_mut();

        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                let frame = data_init.init(frame, ());
                screencopy_state.start_capture(frame, resource, &output, overlay_cursor != 0, None);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let frame = data_init.init(frame, ());
                screencopy_state.start_capture(
                    frame,
                    resource,
                    &output,
                    overlay_cursor != 0,
                    Some((x, y, width, height)),
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {
                // Созданные менеджером кадры остаются валидными (см. протокол).
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------
// Frame Implementation
// ---------------------------------------------------------

impl<D> Dispatch<ZwlrScreencopyFrameV1, (), D> for ScreencopyState
where
    D: Dispatch<ZwlrScreencopyFrameV1, ()> + AsMut<ScreencopyState>,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let screencopy_state = state.as_mut();

        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                screencopy_state.attach_buffer(resource, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                screencopy_state.attach_buffer(resource, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                screencopy_state.remove_frame(resource);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &ZwlrScreencopyFrameV1, _data: &()) {
        // Клиент отключился, не уничтожив кадр: без этого кадр висел бы в
        // очереди вечно, а рендер-цикл будил бы ради него перерисовку.
        state.as_mut().remove_frame(resource);
    }
}

// Helper macros to make delegation easier in state.rs
#[macro_export]
macro_rules! delegate_screencopy {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
        wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1: ()
        ] => $crate::screencopy::ScreencopyState);

        wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1: ()
        ] => $crate::screencopy::ScreencopyState);
    };
}

// ---------------------------------------------------------
// Рендер: offscreen-захват и запись в буфер клиента
// ---------------------------------------------------------

/// Что делать с кадром на этом рендере.
enum FrameAction {
    /// Не наш выход, нет буфера или (copy_with_damage) нет изменений в регионе.
    Wait,
    /// Выход сменил режим/масштаб после объявления буфера — буфер не подходит.
    Fail,
    /// Копировать; для copy_with_damage — с этими damage (в пикселях буфера).
    Copy(Option<Vec<Rectangle<i32, Buffer>>>),
}

/// Ключ накопленного damage: (менеджер, overlay_cursor).
type DamageKey = (Weak<ZwlrScreencopyManagerV1>, bool);

/// Рендер-сторона screencopy для ОДНОГО выхода.
///
/// Держите экземпляр рядом с рендерером и на каждом рендере выхода:
/// 1. до решения «рендерить ли» спросите [`wants_render`](Self::wants_render);
/// 2. после сборки элементов кадра вызовите
///    [`fulfill_pending`](Self::fulfill_pending) — даже если кадров в очереди
///    нет (это дёшево): так `copy_with_damage` не пропустит изменений.
pub struct ScreencopyRenderer {
    /// Offscreen-текстура размером с выход; пересоздаётся только при смене размера.
    texture: Option<GlesTexture>,
    /// Трекеры изменений сцены для copy_with_damage: [без курсора, с курсором].
    trackers: [Option<OutputDamageTracker>; 2],
    /// Damage (в пикселях выхода), накопленный с последнего кадра менеджера.
    /// Записи нет — менеджер ещё не получал кадров: первый отдаём сразу целиком.
    damage: HashMap<DamageKey, Vec<Rectangle<i32, Physical>>>,
    clock: Clock<Monotonic>,
}

impl Default for ScreencopyRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreencopyRenderer {
    pub fn new() -> Self {
        Self {
            texture: None,
            trackers: [None, None],
            damage: HashMap::new(),
            clock: Clock::new(),
        }
    }

    /// Нужно ли отрендерить кадр ради screencopy, даже если сцена не
    /// менялась: есть обычный `copy`, первый `copy_with_damage` менеджера,
    /// накопленный damage в регионе кадра или кадр, который надо провалить.
    pub fn wants_render(&self, state: &ScreencopyState, output: &Output) -> bool {
        state
            .frames()
            .iter()
            .any(|f| !matches!(self.action(f, output), FrameAction::Wait))
    }

    /// Выполняет готовые кадры `output`.
    ///
    /// - `elements` — элементы кадра в порядке front-to-back (как для
    ///   `render_frame`), в координатах выхода;
    /// - первые `cursor_elements` из них — курсор: для клиентов без
    ///   `overlay_cursor` они не рисуются;
    /// - `clear_color` — цвет фона, как у основного рендера.
    ///
    /// Кадры других выходов не трогает. Рендерер после вызова свободен —
    /// можно сразу рисовать основной кадр.
    pub fn fulfill_pending(
        &mut self,
        renderer: &mut GlesRenderer,
        state: &ScreencopyState,
        output: &Output,
        elements: &[CustomRenderElements],
        cursor_elements: usize,
        clear_color: impl Into<Color32F>,
    ) {
        let cursor_elements = cursor_elements.min(elements.len());
        // [без курсора, с курсором]
        let scenes: [&[CustomRenderElements]; 2] = [&elements[cursor_elements..], elements];

        // Менеджеры, которых больше нет, кадров уже не получат.
        self.damage.retain(|(manager, _), _| manager.is_alive());

        let ready = {
            let mut frames = state.frames();
            self.track_damage(output, &scenes, &frames);

            let mut ready = Vec::new();
            let mut i = 0;
            while i < frames.len() {
                match self.action(&frames[i], output) {
                    FrameAction::Wait => i += 1,
                    FrameAction::Fail => {
                        sc_debug!("failed: выход сменил режим");
                        frames.remove(i).frame.failed()
                    }
                    FrameAction::Copy(damage) => ready.push((frames.remove(i), damage)),
                }
            }
            ready
        };
        if ready.is_empty() {
            return;
        }

        let Some(mode) = output.current_mode() else {
            ready.into_iter().for_each(|(f, _)| f.frame.failed());
            return;
        };
        let size = output.current_transform().transform_size(mode.size);
        let scale = output.current_scale().fractional_scale();
        let clear_color = clear_color.into();
        let time: Duration = self.clock.now().into();

        let (with_cursor, without_cursor): (Vec<_>, Vec<_>) =
            ready.into_iter().partition(|(f, _)| f.overlay_cursor);
        for (scene, batch) in [(scenes[0], without_cursor), (scenes[1], with_cursor)] {
            if batch.is_empty() {
                continue;
            }
            let delivered = self.render_offscreen(
                renderer,
                size,
                scale,
                scene,
                clear_color,
                |renderer, target| {
                    batch
                        .iter()
                        .map(|(pending, _)| copy_to_client(renderer, target, pending))
                        .collect::<Vec<_>>()
                },
            );
            match delivered {
                Ok(results) => {
                    for ((pending, damage), result) in batch.into_iter().zip(results) {
                        match result {
                            Ok(()) => {
                                sc_debug!("ready, damage: {:?}", damage.as_ref().map(|d| d.len()));
                                send_ready(&pending.frame, damage.as_deref(), time);
                                self.reset_damage(&pending);
                            }
                            Err(e) => {
                                eprintln!("[screencopy] не удалось скопировать кадр: {e}");
                                pending.frame.failed();
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[screencopy] offscreen-рендер не удался: {e}");
                    batch.into_iter().for_each(|(f, _)| f.frame.failed());
                }
            }
        }
    }

    fn action(&self, f: &PendingFrame, output: &Output) -> FrameAction {
        if f.output != *output || f.buffer.is_none() {
            return FrameAction::Wait;
        }
        if capture_region(output, f.requested_region) != Some(f.region) {
            return FrameAction::Fail;
        }
        if !f.with_damage {
            return FrameAction::Copy(None);
        }
        match self.damage.get(&(f.manager.clone(), f.overlay_cursor)) {
            None => FrameAction::Copy(Some(vec![Rectangle::from_size(
                (f.region.size.w, f.region.size.h).into(),
            )])),
            Some(rects) => {
                let local = local_damage(rects, f.region);
                if local.is_empty() {
                    FrameAction::Wait
                } else {
                    FrameAction::Copy(Some(local))
                }
            }
        }
    }

    /// Прогоняет трекеры изменений и добавляет новый damage всем менеджерам,
    /// которые его копят. Трекер варианта сцены работает, только пока он
    /// кому-то нужен.
    fn track_damage(
        &mut self,
        output: &Output,
        scenes: &[&[CustomRenderElements]; 2],
        frames: &[PendingFrame],
    ) {
        for (variant, scene) in scenes.iter().enumerate() {
            let with_cursor = variant == 1;
            let needed = self.damage.keys().any(|(_, c)| *c == with_cursor)
                || frames.iter().any(|f| {
                    f.with_damage && f.overlay_cursor == with_cursor && f.output == *output
                });
            if !needed {
                continue;
            }
            let tracker = self.trackers[variant]
                .get_or_insert_with(|| OutputDamageTracker::from_output(output));
            let Ok((Some(rects), _)) = tracker.damage_output(1, scene) else {
                continue;
            };
            for ((_, c), acc) in self.damage.iter_mut() {
                if *c == with_cursor {
                    add_damage(acc, rects);
                }
            }
        }
    }

    /// После выданного кадра damage менеджера считается заново. Записи
    /// заводим только тем, кто пользуется copy_with_damage.
    fn reset_damage(&mut self, pending: &PendingFrame) {
        let key = (pending.manager.clone(), pending.overlay_cursor);
        if pending.with_damage || self.damage.contains_key(&key) {
            self.damage.insert(key, Vec::new());
        }
    }

    /// Рисует `scene` в offscreen-текстуру размером `size` и даёт `read`
    /// прочитать из неё регионы.
    fn render_offscreen<T>(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        scale: f64,
        scene: &[CustomRenderElements],
        clear_color: Color32F,
        read: impl FnOnce(&mut GlesRenderer, &GlesTarget<'_>) -> T,
    ) -> Result<T, String> {
        let buffer_size: Size<i32, Buffer> = (size.w, size.h).into();
        let mut texture = match self.texture.take() {
            Some(texture) if texture.size() == buffer_size => texture,
            // RGBA (Abgr8888 в fourcc DRM) поддерживается в GLES везде.
            _ => Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buffer_size)
                .map_err(|e| format!("create_buffer: {e:?}"))?,
        };
        let result = (|| {
            let mut target = renderer
                .bind(&mut texture)
                .map_err(|e| format!("bind: {e:?}"))?;
            // age 0 — каждый раз полная перерисовка: текстура общая для сцен с
            // курсором и без, а захват бывает реже основных кадров.
            OutputDamageTracker::new(size, scale, Transform::Normal)
                .render_output(renderer, &mut target, 0, scene, clear_color)
                .map_err(|e| format!("render: {e:?}"))?;
            Ok(read(renderer, &target))
        })();
        self.texture = Some(texture);
        result
    }
}

/// Читает регион кадра из offscreen-цели и пишет его в shm-буфер клиента.
fn copy_to_client(
    renderer: &mut GlesRenderer,
    target: &GlesTarget<'_>,
    pending: &PendingFrame,
) -> Result<(), String> {
    let region = pending.region;
    let buffer = match pending.buffer.as_ref().ok_or("кадр без буфера")? {
        CaptureBuffer::Shm(buffer) => buffer,
        CaptureBuffer::Dmabuf(dmabuf) => {
            // Строки offscreen-текстуры и dmabuf идут в одном порядке (сверху
            // вниз), так что блит без переворота.
            let mut dmabuf = dmabuf.clone();
            let mut dst = renderer
                .bind(&mut dmabuf)
                .map_err(|e| format!("bind dmabuf: {e:?}"))?;
            renderer
                .blit(
                    target,
                    &mut dst,
                    region,
                    Rectangle::from_size(region.size),
                    TextureFilter::Nearest,
                )
                .map_err(|e| format!("blit: {e:?}"))?;
            // Клиент читает буфер сразу после ready — дожидаемся GPU.
            renderer
                .with_context(|gl| unsafe { gl.Finish() })
                .map_err(|e| format!("glFinish: {e:?}"))?;
            return Ok(());
        }
    };
    let rect = Rectangle::<i32, Buffer>::new(
        (region.loc.x, region.loc.y).into(),
        (region.size.w, region.size.h).into(),
    );
    // Строки offscreen-текстуры уже идут сверху вниз — y-flip не нужен.
    let mapping = renderer
        .copy_framebuffer(target, rect, Fourcc::Abgr8888)
        .map_err(|e| format!("copy_framebuffer: {e:?}"))?;
    let pixels = renderer
        .map_texture(&mapping)
        .map_err(|e| format!("map_texture: {e:?}"))?;
    write_shm(buffer, pixels, region.size)
}

/// Пишет плотно упакованные RGBA-пиксели в shm-буфер с учётом его формата и stride.
fn write_shm(buffer: &WlBuffer, rgba: &[u8], size: Size<i32, Physical>) -> Result<(), String> {
    shm::with_buffer_contents_mut(buffer, |ptr, len, data| {
        // Повторная проверка: буфер мог смениться в пуле между copy и рендером.
        let layout = check_shm_buffer(&data, len, size)?;
        let (width, height, stride) = (size.w as usize, size.h as usize, data.stride as usize);
        let span = stride * (height - 1) + width * 4;
        // SAFETY: check_shm_buffer проверил, что [offset, offset + span) лежит
        // внутри пула; указатель валиден, пока выполняется замыкание.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(data.offset as usize), span) };
        copy_pixels(rgba, width, height, dst, stride, layout)
    })
    .map_err(|e| format!("shm: {e:?}"))?
    .map_err(str::to_string)
}

/// `flags` → (`damage`…) → `ready` с монотонным временем захвата.
fn send_ready(
    frame: &ZwlrScreencopyFrameV1,
    damage: Option<&[Rectangle<i32, Buffer>]>,
    time: Duration,
) {
    frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
    // copy_with_damage появился в v2 вместе с событием damage.
    if let Some(damage) = damage.filter(|_| frame.version() >= 2) {
        for rect in damage {
            frame.damage(
                rect.loc.x as u32,
                rect.loc.y as u32,
                rect.size.w as u32,
                rect.size.h as u32,
            );
        }
    }
    let secs = time.as_secs();
    frame.ready(
        (secs >> 32) as u32,
        (secs & 0xFFFF_FFFF) as u32,
        time.subsec_nanos(),
    );
}

/// Регион захвата в пикселях выхода для текущего режима выхода.
fn capture_region(
    output: &Output,
    region: Option<RequestedRegion>,
) -> Option<Rectangle<i32, Physical>> {
    let mode = output.current_mode()?;
    let size = output.current_transform().transform_size(mode.size);
    clip_region(size, output.current_scale().fractional_scale(), region)
}

/// Переводит логический регион в пиксели выхода и обрезает по его границам
/// (так велит протокол). `None` — захватывать нечего.
fn clip_region(
    output_size: Size<i32, Physical>,
    scale: f64,
    region: Option<RequestedRegion>,
) -> Option<Rectangle<i32, Physical>> {
    if output_size.w <= 0 || output_size.h <= 0 {
        return None;
    }
    let Some((x, y, width, height)) = region else {
        return Some(Rectangle::from_size(output_size));
    };
    if width <= 0 || height <= 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    // Левый/верхний край — вниз, правый/нижний — вверх, чтобы не терять
    // частично покрытые пиксели. Считаем в f64: координаты от клиента могут
    // быть любыми, i32 переполнился бы.
    let clamp = |v: f64, max: i32| v.clamp(0.0, max as f64) as i32;
    let x0 = clamp((x as f64 * scale).floor(), output_size.w);
    let y0 = clamp((y as f64 * scale).floor(), output_size.h);
    let x1 = clamp(((x as f64 + width as f64) * scale).ceil(), output_size.w);
    let y1 = clamp(((y as f64 + height as f64) * scale).ceil(), output_size.h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()))
}

/// Как раскладывать пиксели в буфер клиента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelLayout {
    /// Argb8888 / Xrgb8888 (little-endian): в памяти B,G,R,A.
    Bgra { opaque: bool },
    /// Abgr8888 / Xbgr8888: в памяти R,G,B,A — как отдаёт GL.
    Rgba { opaque: bool },
}

impl PixelLayout {
    fn from_shm(format: wl_shm::Format) -> Option<Self> {
        match format {
            wl_shm::Format::Argb8888 => Some(Self::Bgra { opaque: false }),
            wl_shm::Format::Xrgb8888 => Some(Self::Bgra { opaque: true }),
            wl_shm::Format::Abgr8888 => Some(Self::Rgba { opaque: false }),
            wl_shm::Format::Xbgr8888 => Some(Self::Rgba { opaque: true }),
            _ => None,
        }
    }
}

/// Проверяет, что dmabuf клиента подходит под кадр размером `size`.
fn check_dmabuf(dmabuf: &Dmabuf, size: Size<i32, Physical>) -> Result<Dmabuf, String> {
    let dmabuf_size = dmabuf.size();
    if (dmabuf_size.w, dmabuf_size.h) != (size.w, size.h) {
        return Err("размер dmabuf не совпадает с объявленным".into());
    }
    if !DMABUF_FORMATS.contains(&dmabuf.format().code) {
        return Err(format!(
            "неподдерживаемый формат dmabuf {:?}",
            dmabuf.format().code
        ));
    }
    Ok(dmabuf.clone())
}

/// Проверяет, что shm-буфер клиента подходит под кадр размером `size`.
fn check_shm_buffer(
    data: &BufferData,
    pool_len: usize,
    size: Size<i32, Physical>,
) -> Result<PixelLayout, &'static str> {
    let layout = PixelLayout::from_shm(data.format).ok_or("неподдерживаемый формат wl_shm")?;
    if data.width != size.w || data.height != size.h {
        return Err("размер буфера не совпадает с объявленным");
    }
    let (width, height) = (data.width as i64, data.height as i64);
    let (stride, offset) = (data.stride as i64, data.offset as i64);
    if width <= 0 || height <= 0 {
        return Err("пустой буфер");
    }
    if stride < width * 4 {
        return Err("stride меньше длины строки");
    }
    if offset < 0 || offset + stride * (height - 1) + width * 4 > pool_len as i64 {
        return Err("буфер выходит за пределы пула");
    }
    Ok(layout)
}

/// Копирует плотно упакованные RGBA-строки (`width`×`height`) в `dst` со
/// stride `dst_stride`, переставляя каналы под `layout`. Байты между концом
/// строки и stride не трогает.
fn copy_pixels(
    src: &[u8],
    width: usize,
    height: usize,
    dst: &mut [u8],
    dst_stride: usize,
    layout: PixelLayout,
) -> Result<(), &'static str> {
    let row = width * 4;
    if row == 0 || height == 0 {
        return Ok(());
    }
    if dst_stride < row {
        return Err("stride меньше длины строки");
    }
    if src.len() < row * height {
        return Err("GPU вернул меньше пикселей, чем нужно");
    }
    if dst.len() < dst_stride * (height - 1) + row {
        return Err("буфер клиента меньше кадра");
    }
    for (src_row, dst_row) in src
        .chunks_exact(row)
        .take(height)
        .zip(dst.chunks_mut(dst_stride))
    {
        convert_row(src_row, &mut dst_row[..row], layout);
    }
    Ok(())
}

/// Одна строка RGBA → формат клиента. `chunks_exact(4)` без проверок границ
/// на каждый пиксель — компилятор векторизует цикл.
fn convert_row(src: &[u8], dst: &mut [u8], layout: PixelLayout) {
    match layout {
        PixelLayout::Bgra { opaque } => {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = if opaque { 0xFF } else { s[3] };
            }
        }
        PixelLayout::Rgba { opaque: false } => dst.copy_from_slice(src),
        PixelLayout::Rgba { opaque: true } => {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                d[..3].copy_from_slice(&s[..3]);
                d[3] = 0xFF;
            }
        }
    }
}

/// Добавляет damage, не давая списку разрастись: уже покрытые прямоугольники
/// пропускаем, а слишком длинный список схлопываем в один.
fn add_damage(acc: &mut Vec<Rectangle<i32, Physical>>, rects: &[Rectangle<i32, Physical>]) {
    for rect in rects {
        if !acc.iter().any(|known| known.contains_rect(*rect)) {
            acc.push(*rect);
        }
    }
    if acc.len() > MAX_DAMAGE_RECTS {
        let merged = acc.iter().copied().reduce(|a, b| a.merge(b));
        acc.clear();
        acc.extend(merged);
    }
}

/// Damage в координатах буфера кадра: пересечение с регионом, сдвинутое к его началу.
fn local_damage(
    rects: &[Rectangle<i32, Physical>],
    region: Rectangle<i32, Physical>,
) -> Vec<Rectangle<i32, Buffer>> {
    rects
        .iter()
        .filter_map(|rect| rect.intersection(region))
        .map(|r| {
            Rectangle::new(
                (r.loc.x - region.loc.x, r.loc.y - region.loc.y).into(),
                (r.size.w, r.size.h).into(),
            )
        })
        .collect()
}

/// Headless GLES (Mesa llvmpipe через EGL device platform), если доступен.
#[cfg(test)]
pub(crate) fn headless_renderer() -> Option<GlesRenderer> {
    use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
    let device = EGLDevice::enumerate().ok()?.find(|d| {
        d.extensions()
            .iter()
            .any(|e| e == "EGL_MESA_device_software")
            || d.try_get_render_node().ok().flatten().is_some()
    })?;
    let display = unsafe { EGLDisplay::new(device) }.ok()?;
    let context = EGLContext::new(&display).ok()?;
    unsafe { GlesRenderer::new(context) }.ok()
}

/// Рисует элементы offscreen тем же путём, что и screencopy, и возвращает
/// весь кадр плотными RGBA-строками.
#[cfg(test)]
pub(crate) fn render_for_tests(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    elements: &[CustomRenderElements],
    clear_color: [f32; 4],
) -> Vec<u8> {
    ScreencopyRenderer::new()
        .render_offscreen(
            renderer,
            size,
            1.0,
            elements,
            clear_color.into(),
            |renderer, target| {
                let rect = Rectangle::<i32, Buffer>::from_size((size.w, size.h).into());
                let mapping = renderer
                    .copy_framebuffer(target, rect, Fourcc::Abgr8888)
                    .expect("copy_framebuffer");
                renderer
                    .map_texture(&mapping)
                    .expect("map_texture")
                    .to_vec()
            },
        )
        .expect("offscreen render")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect<K>(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, K> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn shm_data(format: wl_shm::Format, w: i32, h: i32, stride: i32, offset: i32) -> BufferData {
        BufferData {
            offset,
            width: w,
            height: h,
            stride,
            format,
        }
    }

    #[test]
    fn clip_region_full_and_partial() {
        let size: Size<i32, Physical> = (1920, 1080).into();
        assert_eq!(clip_region(size, 1.0, None), Some(rect(0, 0, 1920, 1080)));
        assert_eq!(
            clip_region(size, 1.0, Some((100, 50, 640, 480))),
            Some(rect(100, 50, 640, 480))
        );
        // Частично за границей — обрезаем.
        assert_eq!(
            clip_region(size, 1.0, Some((-10, 1000, 100, 200))),
            Some(rect(0, 1000, 90, 80))
        );
        // Целиком снаружи или пустой — захватывать нечего.
        assert_eq!(clip_region(size, 1.0, Some((2000, 0, 10, 10))), None);
        assert_eq!(clip_region(size, 1.0, Some((0, 0, 0, 10))), None);
        assert_eq!(clip_region(size, 1.0, Some((0, 0, -5, 10))), None);
        assert_eq!(clip_region((0, 0).into(), 1.0, None), None);
        // Мусорные координаты не переполняют i32.
        assert_eq!(
            clip_region(size, 1.0, Some((i32::MAX - 1, 0, i32::MAX, 10))),
            None
        );
        assert_eq!(
            clip_region(size, 1.0, Some((i32::MIN, i32::MIN, i32::MAX, i32::MAX))),
            None
        );
        assert_eq!(
            clip_region(size, 1.0, Some((-5, -5, i32::MAX, i32::MAX))),
            Some(rect(0, 0, 1920, 1080))
        );
    }

    #[test]
    fn clip_region_scaled() {
        let size: Size<i32, Physical> = (3840, 2160).into();
        assert_eq!(
            clip_region(size, 2.0, Some((10, 20, 100, 50))),
            Some(rect(20, 40, 200, 100))
        );
        // Дробный масштаб: края расширяются до целых пикселей.
        assert_eq!(
            clip_region(size, 1.5, Some((1, 1, 3, 3))),
            Some(rect(1, 1, 5, 5))
        );
    }

    #[test]
    fn shm_buffer_validation() {
        let size: Size<i32, Physical> = (3, 2).into();
        let ok = |format, stride, offset, pool| {
            check_shm_buffer(&shm_data(format, 3, 2, stride, offset), pool, size)
        };
        assert_eq!(
            ok(wl_shm::Format::Argb8888, 12, 0, 24),
            Ok(PixelLayout::Bgra { opaque: false })
        );
        assert_eq!(
            ok(wl_shm::Format::Xrgb8888, 16, 4, 4 + 16 + 12),
            Ok(PixelLayout::Bgra { opaque: true })
        );
        assert_eq!(
            ok(wl_shm::Format::Abgr8888, 12, 0, 24),
            Ok(PixelLayout::Rgba { opaque: false })
        );
        assert_eq!(
            ok(wl_shm::Format::Xbgr8888, 12, 0, 24),
            Ok(PixelLayout::Rgba { opaque: true })
        );
        // Последняя строка не обязана занимать весь stride.
        assert!(ok(wl_shm::Format::Argb8888, 20, 0, 20 + 12).is_ok());

        assert!(ok(wl_shm::Format::Rgb565, 12, 0, 24).is_err());
        assert!(ok(wl_shm::Format::Argb8888, 8, 0, 24).is_err());
        assert!(ok(wl_shm::Format::Argb8888, 12, 0, 23).is_err());
        assert!(ok(wl_shm::Format::Argb8888, 12, -4, 24).is_err());
        assert!(ok(wl_shm::Format::Argb8888, 12, 4, 24).is_err());
        let wrong_size = shm_data(wl_shm::Format::Argb8888, 4, 2, 16, 0);
        assert!(check_shm_buffer(&wrong_size, 1000, size).is_err());
    }

    /// RGBA-пиксель с узнаваемыми каналами для (x, y).
    fn rgba(x: usize, y: usize) -> [u8; 4] {
        [x as u8 * 10 + 1, y as u8 * 10 + 2, 0x80, 0x40]
    }

    fn src_image(w: usize, h: usize) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).flat_map(move |x| rgba(x, y)))
            .collect()
    }

    #[test]
    fn copy_pixels_swizzles_and_respects_stride() {
        let (w, h, stride) = (3, 4, 3 * 4 + 7);
        let src = src_image(w, h);
        let mut dst = vec![0xEE; stride * (h - 1) + w * 4];
        copy_pixels(
            &src,
            w,
            h,
            &mut dst,
            stride,
            PixelLayout::Bgra { opaque: false },
        )
        .unwrap();
        for y in 0..h {
            for x in 0..w {
                let i = y * stride + x * 4;
                let [r, g, b, a] = rgba(x, y);
                assert_eq!(dst[i..i + 4], [b, g, r, a], "пиксель ({x}, {y})");
            }
            // Хвост строки (padding stride) не тронут.
            if y + 1 < h {
                assert!(
                    dst[y * stride + w * 4..(y + 1) * stride]
                        .iter()
                        .all(|&v| v == 0xEE)
                );
            }
        }
    }

    #[test]
    fn copy_pixels_all_layouts() {
        let (w, h) = (5, 2);
        let src = src_image(w, h);
        let run = |layout| {
            let mut dst = vec![0u8; w * h * 4];
            copy_pixels(&src, w, h, &mut dst, w * 4, layout).unwrap();
            dst
        };
        let px = |buf: &[u8], x: usize, y: usize| -> [u8; 4] {
            let i = (y * w + x) * 4;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        let [r, g, b, a] = rgba(4, 1);
        assert_eq!(
            px(&run(PixelLayout::Bgra { opaque: false }), 4, 1),
            [b, g, r, a]
        );
        assert_eq!(
            px(&run(PixelLayout::Bgra { opaque: true }), 4, 1),
            [b, g, r, 0xFF]
        );
        assert_eq!(
            px(&run(PixelLayout::Rgba { opaque: false }), 4, 1),
            [r, g, b, a]
        );
        assert_eq!(
            px(&run(PixelLayout::Rgba { opaque: true }), 4, 1),
            [r, g, b, 0xFF]
        );
    }

    #[test]
    fn copy_pixels_rejects_short_buffers() {
        let src = src_image(4, 4);
        let layout = PixelLayout::Bgra { opaque: false };
        let mut small = vec![0u8; 4 * 4 * 4 - 1];
        assert!(copy_pixels(&src, 4, 4, &mut small, 16, layout).is_err());
        let mut ok = vec![0u8; 64];
        assert!(copy_pixels(&src, 4, 4, &mut ok, 15, layout).is_err());
        assert!(copy_pixels(&src[..10], 4, 4, &mut ok, 16, layout).is_err());
        assert!(copy_pixels(&src, 0, 4, &mut ok, 16, layout).is_ok());
    }

    #[test]
    fn damage_accumulates_and_clips_to_region() {
        let mut acc = Vec::new();
        add_damage(&mut acc, &[rect(0, 0, 10, 10), rect(100, 100, 5, 5)]);
        let region = rect::<Physical>(5, 5, 50, 50);
        assert_eq!(local_damage(&acc, region), vec![rect(0, 0, 5, 5)]);
        assert!(local_damage(&acc, rect(200, 200, 10, 10)).is_empty());

        // Уже покрытые прямоугольники (тот же damage с двух рендеров) не копятся.
        add_damage(&mut acc, &[rect(0, 0, 10, 10), rect(2, 2, 3, 3)]);
        assert_eq!(acc.len(), 2);

        // Слишком много прямоугольников схлопываются в охватывающий.
        let many: Vec<_> = (0..MAX_DAMAGE_RECTS as i32 + 1)
            .map(|i| rect(20 + i * 2, 0, 1, 1))
            .collect();
        add_damage(&mut acc, &many);
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0], rect(0, 0, 105, 105));
    }

    #[test]
    fn offscreen_capture_regions_and_orientation() {
        use smithay::backend::renderer::element::Kind;
        use smithay::backend::renderer::element::solid::{
            SolidColorBuffer, SolidColorRenderElement,
        };

        let Some(mut renderer) = headless_renderer() else {
            eprintln!("нет headless EGL — тест пропущен");
            return;
        };
        let size: Size<i32, Physical> = (64, 48).into();
        let red = SolidColorBuffer::new((20, 10), [1.0, 0.0, 0.0, 1.0]);
        let blue = SolidColorBuffer::new((64, 8), [0.0, 0.0, 1.0, 1.0]);
        let green = SolidColorBuffer::new((4, 4), [0.0, 1.0, 0.0, 1.0]);
        let elements = vec![
            // «Курсор» поверх всего.
            CustomRenderElements::Cursor(SolidColorRenderElement::from_buffer(
                &green,
                (12, 6),
                1.0,
                1.0,
                Kind::Cursor,
            )),
            CustomRenderElements::Cursor(SolidColorRenderElement::from_buffer(
                &red,
                (10, 5),
                1.0,
                1.0,
                Kind::Unspecified,
            )),
            CustomRenderElements::Cursor(SolidColorRenderElement::from_buffer(
                &blue,
                (0, 40),
                1.0,
                1.0,
                Kind::Unspecified,
            )),
        ];
        let clear = Color32F::from([0.0, 0.0, 0.0, 1.0]);

        let read =
            |renderer: &mut GlesRenderer, target: &GlesTarget<'_>, r: Rectangle<i32, Buffer>| {
                let mapping = renderer
                    .copy_framebuffer(target, r, Fourcc::Abgr8888)
                    .unwrap();
                renderer.map_texture(&mapping).unwrap().to_vec()
            };
        let mut sc = ScreencopyRenderer::new();
        for (scene, cursor) in [(&elements[1..], false), (&elements[..], true)] {
            let (top, bottom) = sc
                .render_offscreen(
                    &mut renderer,
                    size,
                    1.0,
                    scene,
                    clear,
                    |renderer, target| {
                        (
                            read(renderer, target, rect(8, 4, 16, 8)),
                            read(renderer, target, rect(0, 36, 64, 12)),
                        )
                    },
                )
                .unwrap();
            let px = |buf: &[u8], w: usize, x: usize, y: usize| -> [u8; 4] {
                let i = (y * w + x) * 4;
                [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
            };
            // Регион (8,4): (0,0) — фон, (2,1) = абсолютные (10,5) — красный.
            assert_eq!(px(&top, 16, 0, 0), [0, 0, 0, 255]);
            assert_eq!(px(&top, 16, 2, 1), [255, 0, 0, 255]);
            // (4,2) = абсолютные (12,6): курсор только в сцене с курсором.
            let expected = if cursor {
                [0, 255, 0, 255]
            } else {
                [255, 0, 0, 255]
            };
            assert_eq!(px(&top, 16, 4, 2), expected);
            // Регион (0,36): строки 0..4 — фон, 4..12 — синяя полоса (нет y-flip).
            assert_eq!(px(&bottom, 64, 30, 3), [0, 0, 0, 255]);
            assert_eq!(px(&bottom, 64, 30, 4), [0, 0, 255, 255]);
            assert_eq!(px(&bottom, 64, 30, 11), [0, 0, 255, 255]);
        }

        // Путь dmabuf: блит региона в отдельную цель (здесь — текстуру) без
        // переворота, как и чтение в shm.
        let region = rect::<Physical>(0, 36, 64, 12);
        let mut dst_tex = Offscreen::<GlesTexture>::create_buffer(
            &mut renderer,
            Fourcc::Abgr8888,
            (64, 12).into(),
        )
        .unwrap();
        sc.render_offscreen(
            &mut renderer,
            size,
            1.0,
            &elements,
            clear,
            |renderer, target| {
                let mut dst = renderer.bind(&mut dst_tex).unwrap();
                renderer
                    .blit(
                        target,
                        &mut dst,
                        region,
                        Rectangle::from_size(region.size),
                        TextureFilter::Nearest,
                    )
                    .unwrap();
            },
        )
        .unwrap();
        let blitted = {
            let target = renderer.bind(&mut dst_tex).unwrap();
            read(&mut renderer, &target, rect(0, 0, 64, 12))
        };
        let px = |x: usize, y: usize| -> [u8; 4] {
            let i = (y * 64 + x) * 4;
            [blitted[i], blitted[i + 1], blitted[i + 2], blitted[i + 3]]
        };
        assert_eq!(px(30, 3), [0, 0, 0, 255]);
        assert_eq!(px(30, 4), [0, 0, 255, 255]);
        assert_eq!(px(30, 11), [0, 0, 255, 255]);

        // Текстура переиспользуется при том же размере и пересоздаётся при другом.
        let first = sc.texture.as_ref().map(|t| t.tex_id());
        sc.render_offscreen(&mut renderer, size, 1.0, &elements, clear, |_, _| ())
            .unwrap();
        assert_eq!(sc.texture.as_ref().map(|t| t.tex_id()), first);
        sc.render_offscreen(
            &mut renderer,
            (32, 32).into(),
            1.0,
            &elements,
            clear,
            |_, _| (),
        )
        .unwrap();
        assert_eq!(sc.texture.as_ref().map(|t| t.size()), Some((32, 32).into()));
    }
}
