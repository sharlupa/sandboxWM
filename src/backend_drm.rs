/// DRM/KMS + libinput backend — запуск sandboxWM прямо из TTY.
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::cursor::CursorRenderer;
use crate::render::CustomRenderElements;
use crate::screencopy::ScreencopyRenderer;
use crate::state::AppState;
use smithay::reexports::rustix;
use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::gles::GlesRenderer,
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{EventLoop, timer::Timer},
        drm::control::{Device as ControlDevice, ModeTypeFlags, connector},
        wayland_server::Display,
    },
    utils::{DeviceFd, Transform},
};

pub fn run_tty(
    mut event_loop: EventLoop<AppState>,
    mut display: Display<AppState>,
    state: &mut AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let loop_handle = event_loop.handle();
    let display_handle = state.display_handle.clone();

    // 1. libseat сессия
    let (session, notifier) = LibSeatSession::new().map_err(|e| format!("libseat: {e}"))?;
    let seat_name = session.seat();
    println!("=====> Сессия: {seat_name}");
    state.session = Some(session.clone());

    // 2. GPU
    // primary_gpu ищет boot_vga; на машинах без него (ARM, virtio) берём первую GPU.
    let gpu_path = match primary_gpu(&seat_name)? {
        Some(path) => path,
        None => all_gpus(&seat_name)?
            .into_iter()
            .next()
            .ok_or_else(|| "Видеокарта не найдена".to_string())?,
    };
    println!("=====> GPU: {gpu_path:?}");

    // 3. DRM fd
    let drm_raw_fd = session.clone().open(
        &gpu_path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOCTTY
            | rustix::fs::OFlags::NONBLOCK,
    )?;
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(drm_raw_fd));
    let (mut drm, drm_notifier) = DrmDevice::new(drm_fd.clone(), true)?;
    let gbm = GbmDevice::new(drm_fd.clone())?;
    // Явная синхронизация буферов (NVIDIA: без неё XWayland копирует каждый
    // кадр игры). До первой обработки клиентов — XWayland ещё не прочитал
    // реестр.
    state.enable_explicit_sync(drm_fd.clone());

    // 4. EGL + OpenGL
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };

    // 5. Монитор
    let res = drm.resource_handles()?;
    let connector = res
        .connectors()
        .iter()
        .filter_map(|&h| drm.get_connector(h, false).ok())
        .find(|c| c.state() == connector::State::Connected)
        .ok_or_else(|| "Нет подключённого монитора".to_string())?;

    println!("=====> Монитор: {:?}", connector.interface());

    let drm_mode = connector
        .modes()
        .iter()
        .max_by_key(|m| {
            let pref = if m.mode_type().contains(ModeTypeFlags::PREFERRED) {
                1_000_000u32
            } else {
                0
            };
            pref + m.size().0 as u32 * m.size().1 as u32
        })
        .copied()
        .ok_or_else(|| "Нет режимов монитора".to_string())?;

    println!(
        "=====> Режим: {}x{}@{}Hz",
        drm_mode.size().0,
        drm_mode.size().1,
        drm_mode.vrefresh()
    );

    // Энкодер + CRTC
    let encoder = connector
        .current_encoder()
        .and_then(|h| drm.get_encoder(h).ok())
        .or_else(|| {
            connector
                .encoders()
                .iter()
                .find_map(|&h| drm.get_encoder(h).ok())
        })
        .ok_or_else(|| "Нет энкодера".to_string())?;

    let crtc = res
        .filter_crtcs(encoder.possible_crtcs())
        .into_iter()
        .next()
        .ok_or_else(|| "Нет CRTC".to_string())?;

    // 6. smithay Output
    // From<drm Mode> считает частоту точно (59.94 Гц → 59940 мГц), а не vrefresh * 1000.
    let smithay_mode = Mode::from(drm_mode);
    let frame_interval = frame_interval(smithay_mode.refresh);
    let output = Output::new(
        format!("{:?}", connector.interface()),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Generic".into(),
            model: "Monitor".into(),
        },
    );
    let _global = output.create_global::<AppState>(&display_handle);
    output.change_current_state(
        Some(smithay_mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(smithay_mode);
    state.space.map_output(&output, (0, 0));
    // Выход есть — можно грузить сессию (пол/камера зависят от размера экрана).
    state.on_output_ready();

    // 7. DRM поверхность + DrmCompositor
    let drm_surface = drm.create_surface(crtc, drm_mode, &[connector.handle()])?;
    let drm_node = DrmNode::from_path(&gpu_path).ok();
    let exporter = GbmFramebufferExporter::new(gbm.clone(), drm_node);
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let color_fmts = [Fourcc::Abgr8888, Fourcc::Argb8888];
    let render_fmts = renderer.egl_context().dmabuf_render_formats().clone();

    let compositor: Rc<RefCell<DrmCompositor<_, _, (), _>>> =
        Rc::new(RefCell::new(DrmCompositor::new(
            &output,
            drm_surface,
            None,
            allocator,
            exporter,
            color_fmts,
            render_fmts,
            drm.cursor_size(),
            Some(gbm.clone()),
        )?));

    println!("=====> DRM compositor готов!");

    // DMA-BUF (zero-copy): advertise the linux-dmabuf global so GPU clients
    // (Alacritty, Kitty, browsers...) can hand us video memory directly instead
    // of round-tripping pixels through wl_shm / RAM.
    //
    // Also bind the renderer's EGL buffer reader to our Wayland display. On
    // Mesa this gives clients the EGL wl_drm interface and lets the GLES
    // renderer import their dmabufs as textures — the actual zero-copy path.
    use smithay::backend::renderer::{ImportDma, ImportEgl};
    match renderer.bind_wl_display(&display_handle) {
        Ok(()) => println!("=====> EGL hardware-acceleration enabled"),
        Err(e) => eprintln!("[DRM] EGL bind_wl_display failed: {e:?}"),
    }

    let dmabuf_formats = renderer.dmabuf_formats();
    state.screencopy_state.set_renderer(&renderer);
    // The GBM device wraps the same DRM node we render on; its dev_id is the
    // main device clients should target when allocating buffers for us.
    let main_device = DrmNode::from_path(&gpu_path)
        .map(|n| n.dev_id())
        .unwrap_or_else(|_| drm_fd.dev_id().unwrap_or(0));
    let dmabuf_global =
        match smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(main_device, dmabuf_formats)
            .build()
        {
            Ok(default_feedback) => state
                .dmabuf_state
                .create_global_with_default_feedback::<AppState>(
                    &display_handle,
                    &default_feedback,
                ),
            Err(e) => {
                // Без feedback (v4) остаётся v3-глобал с одним списком форматов.
                eprintln!("[DRM] dmabuf feedback не собран ({e:?}), fallback на v3");
                state
                    .dmabuf_state
                    .create_global::<AppState>(&display_handle, renderer.dmabuf_formats())
            }
        };
    state.dmabuf_global = Some(dmabuf_global);
    println!("=====> DMA-BUF (zero-copy) global создан");

    // 7.4. libinput
    use smithay::reexports::input as libinput_raw;
    let mut libinput_ctx =
        libinput_raw::Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput_ctx
        .udev_assign_seat(&seat_name)
        .map_err(|()| format!("libinput: не удалось назначить seat {seat_name}"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_ctx.clone());
    loop_handle.insert_source(libinput_backend, |event, _, state| {
        crate::input::process_event(state, event);
    })?;

    // Кадры на выходе. Рендерер, курсор и screencopy живут только в
    // замыканиях event loop (vblank, таймер, сессия) — порядок освобождения
    // при выходе прежний (см. комментарий в конце функции).
    let frames = Rc::new(RefCell::new(OutputFrames {
        renderer,
        compositor: compositor.clone(),
        output: output.clone(),
        // Курсор: клиентская поверхность / xcursor-тема / встроенная стрелка.
        // Элементы с Kind::Cursor DrmCompositor сам кладёт на аппаратную
        // cursor-плоскость (DrmCompositor создан с cursor_size и gbm).
        cursor_renderer: CursorRenderer::new(),
        // Screencopy: кэш offscreen-текстуры и damage для copy_with_damage.
        screencopy: ScreencopyRenderer::new(),
        frame_interval,
        pending_since: None,
        stats: FrameStats::from_env(),
    }));

    // 7.5. Обработка событий сессии (VT-переключения)
    // DrmDevice переезжает сюда: на паузе он отдаёт DRM master и помечает
    // поверхности неактивными, при возврате — забирает master обратно. Его
    // DrmDeviceInternal делят с ним поверхности компоновщика, так что порядок
    // освобождения при выходе не меняется (см. комментарий в конце функции).
    let compositor_session = compositor.clone();
    let frames_session = frames.clone();
    let mut libinput_session = libinput_ctx;
    let mut drm_session = drm;
    loop_handle.insert_source(notifier, move |event, _, state| match event {
        SessionEvent::ActivateSession => {
            state.session_paused = false;
            state.needs_render = true;
            if let Err(e) = libinput_session.resume() {
                eprintln!("[libinput] Ошибка при resume: {:?}", e);
            }
            if let Err(e) = drm_session.activate(false) {
                eprintln!("[DRM] Ошибка при активации устройства: {e:?}");
            }
            if let Err(e) = compositor_session.borrow_mut().reset_state() {
                eprintln!(
                    "[DRM] Ошибка при сбросе состояния после активации сессии: {:?}",
                    e
                );
            }
            // vblank кадра, отданного до паузы, уже не придёт.
            frames_session.borrow_mut().pending_since = None;
            println!("[session] активна");
        }
        SessionEvent::PauseSession => {
            state.session_paused = true;
            libinput_session.suspend();
            drm_session.pause();
            println!("[session] пауза");
        }
    })?;

    // 8. VBlank: кадр показан — подтверждаем его и сразу рисуем следующий,
    //    чтобы клиенты получили frame callback в начале периода обновления.
    let frames_vb = frames.clone();
    loop_handle.insert_source(drm_notifier, move |event, _, state| {
        if let DrmEvent::VBlank(_) = event {
            let mut frames = frames_vb.borrow_mut();
            frames.compositor.borrow_mut().frame_submitted().ok();
            frames.pending_since = None;
            if let Some(stats) = frames.stats.as_mut() {
                stats.vblank(Instant::now());
            }
            frames.render_if_needed(state);
        }
    })?;

    // 10. udev hotplug
    let udev_backend = UdevBackend::new(&seat_name)?;
    loop_handle.insert_source(udev_backend, |event, _, _| match event {
        UdevEvent::Added { path, .. } => println!("[udev] GPU добавлена: {path:?}"),
        UdevEvent::Changed { .. } => {}
        UdevEvent::Removed { device_id } => println!("[udev] GPU удалена: {device_id}"),
    })?;

    // 11. Таймер с периодом обновления монитора будит рендер, пока экран
    //     простаивает (кадр не ждёт vblank); пока кадры идут, следующий
    //     рисуется по vblank прошлого. Без изменений (`state.needs_render`)
    //     кадр не рисуется: простой стол стоит ~0 GPU/CPU.
    loop_handle.insert_source(Timer::from_duration(frame_interval), move |_, _, state| {
        frames.borrow_mut().render_if_needed(state);
        smithay::reexports::calloop::timer::TimeoutAction::ToDuration(frame_interval)
    })?;

    // ── Main dispatch loop ──────────────────────────────────────────────────
    while state.running {
        event_loop.dispatch(Duration::from_millis(16), state)?;
        display.dispatch_clients(state)?;
        state.flush_pointer_refresh();
        display.flush_clients()?;
    }

    // ── Explicit cleanup (correct destruction order) ────────────────────────
    //
    // The key invariant: DRM/EGL/GBM native resources must be destroyed
    // BEFORE the libseat session closes (which revokes the DRM master fd).
    // Calling any EGL/DRM/GBM function on a revoked fd → SIGSEGV in the
    // native driver (not a Rust panic — it's FFI).
    //
    // event_loop owns all calloop sources whose closures captured:
    //   • `renderer` (GlesRenderer, inside OutputFrames shared by the vblank,
    //     render-timer and session closures)
    //   • `compositor_*` (Rc<RefCell<DrmCompositor>> clones)
    //   • libinput backend (holds a session clone)
    //
    // Dropping event_loop releases those closures.  Their Drop impls call
    // eglDestroyContext, drmModeRmFB, libinput_unref, etc.  At this point
    // all DRM locals in this function scope (drm, gbm, egl_display, the
    // original compositor Rc) are still alive, keeping the DRM fd valid.
    //
    // After event_loop, we drop display (Wayland server teardown — no DRM).
    //
    // Then this function returns.  Rust drops locals in reverse declaration
    // order: compositor Rc (last ref → DrmCompositor::drop → DRM ioctls),
    // egl_display (EGL teardown), gbm (GBM device destroy), drm (DRM
    // device cleanup), session clone (Arc decrement, NOT the last ref).
    //
    // Finally, back in main(), `state` drops.  state.session (the last
    // LibSeatSession Arc) drops → libseat_close_seat() → fd revoked.
    // By that point every native resource is already gone.  No SIGSEGV.

    // Сессию сохраняем, пока физический мир ещё жив.
    state.save_session();

    // Clear physics state while everything is alive (avoids rapier handles
    // referencing a partially-torn-down world during field-order drops).
    state.pieces.clear();
    state.physics = None;
    state.workspaces.clear();

    drop(loop_handle);
    drop(event_loop);
    drop(display);

    // Locals (compositor, egl_display, gbm, session clone) drop here
    // in reverse declaration order — all safe because the DRM fd is still
    // valid (state.session is the last Arc ref, dropped later in main()).
    // The DrmDevice itself was moved into the session closure and dropped
    // with event_loop; its DrmDeviceInternal is shared with the compositor's
    // surface, so the device teardown still happens with the compositor.

    Ok(())
}

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// Кадры на выходе: рендер сразу после vblank прошлого кадра.
///
/// Раньше кадры рисовал таймер с периодом обновления монитора, перезаводимый
/// после каждого рендера: настоящий период выходил длиннее на время рендера
/// и не совпадал с vblank по фазе. Клиенты (и XWayland за игры) рисуют
/// следующий кадр по нашему frame callback — игры получали меньше кадров,
/// чем частота монитора, а биение таймера с vblank давало повторы кадров
/// (заметные подёргивания).
struct OutputFrames {
    // Рендерер освобождается раньше компоновщика, как и раньше.
    renderer: GlesRenderer,
    compositor: Rc<RefCell<GbmDrmCompositor>>,
    output: Output,
    cursor_renderer: CursorRenderer,
    screencopy: ScreencopyRenderer,
    frame_interval: Duration,
    /// Кадр отдан DRM и ждёт vblank — с этого момента.
    pending_since: Option<Instant>,
    /// `SANDBOXWM_DEBUG_FRAMES=1`: статистика кадров раз в секунду.
    stats: Option<FrameStats>,
}

/// Статистика кадров за секунду (`SANDBOXWM_DEBUG_FRAMES=1`): сколько
/// кадров показано и сколько из них — буфер клиента прямо на экране
/// (scanout, без перерисовки), самый долгий рендер и промежуток между
/// vblank. Диагностика FPS и подёргиваний в играх.
#[derive(Default)]
struct FrameStats {
    since: Option<Instant>,
    last_vblank: Option<Instant>,
    vblanks: u32,
    max_vblank_gap: Duration,
    frames: u32,
    scanout: u32,
    max_render: Duration,
    max_elements: usize,
}

impl FrameStats {
    fn from_env() -> Option<Self> {
        std::env::var_os("SANDBOXWM_DEBUG_FRAMES")
            .is_some_and(|v| v != "0")
            .then(Self::default)
    }

    fn vblank(&mut self, now: Instant) {
        if let Some(last) = self.last_vblank.replace(now) {
            self.max_vblank_gap = self.max_vblank_gap.max(now - last);
        }
        self.vblanks += 1;
        let since = *self.since.get_or_insert(now);
        if now - since >= Duration::from_secs(1) {
            println!(
                "[frames] vblank {} (макс. промежуток {:.1} мс), кадров {}, напрямую {}, \
                 рендер до {:.1} мс, элементов до {}",
                self.vblanks,
                self.max_vblank_gap.as_secs_f64() * 1000.0,
                self.frames,
                self.scanout,
                self.max_render.as_secs_f64() * 1000.0,
                self.max_elements
            );
            *self = Self {
                since: Some(now),
                last_vblank: Some(now),
                ..Self::default()
            };
        }
    }

    fn frame(&mut self, scanout: bool, render: Duration, elements: usize) {
        self.frames += 1;
        self.scanout += u32::from(scanout);
        self.max_render = self.max_render.max(render);
        self.max_elements = self.max_elements.max(elements);
    }
}

impl OutputFrames {
    /// Нарисовать кадр, если что-то изменилось и экран не ждёт vblank
    /// прошлого кадра.
    fn render_if_needed(&mut self, state: &mut AppState) {
        if let Some(since) = self.pending_since {
            // vblank потерялся (выход выключен, флип не удался) — не ждём
            // его вечно.
            if since.elapsed() < self.frame_interval * 4 {
                return;
            }
            self.pending_since = None;
        }

        // ── Физический режим: шаг симуляции ─────────────
        // Выполняем шаг ДО проверки `needs_render`. Если тела движутся,
        // `step_physics` сам выставит `needs_render = true`, разбудив рендер.
        state.step_physics();

        // Screencopy-клиент (OBS через portal-wlr) ждёт кадр — будим рендер,
        // даже если сцена статична: иначе захват замирает/чернеет.
        // copy_with_damage будит рендер, только когда в его регионе что-то
        // изменилось.
        if self
            .screencopy
            .wants_render(&state.screencopy_state, &self.output)
        {
            state.needs_render = true;
        }
        // Указатель сдвинулся, клиент сменил курсор или анимированный
        // курсор перешёл на следующий кадр.
        if self
            .cursor_renderer
            .needs_redraw(&state.cursor_status, state.pointer_location)
        {
            state.needs_render = true;
        }
        if state.session_paused || !state.needs_render {
            return;
        }

        let Self {
            renderer,
            compositor,
            output,
            cursor_renderer,
            screencopy,
            pending_since,
            stats,
            ..
        } = self;
        let started = Instant::now();

        // Process deferred layout (resize, etc.) once per frame.
        if state.layout_dirty {
            state.recalculate_layout();
            state.layout_dirty = false;
        }

        // ── Смещение камеры (Tiling = (0,0), Physics = Lerp) ─────────────
        // map_output смещает вид: world = screen + camera_offset.
        state.space.map_output(
            output,
            (state.camera_offset.0 as i32, state.camera_offset.1 as i32),
        );

        // Окна + оверлей физики (линии, верёвки, связи, пол) — общий
        // сборщик кадра для обоих бэкендов.
        let mut elements: Vec<CustomRenderElements> =
            crate::render::frame_elements(renderer, state, output);

        // Курсор поверх окон. `render_frame` принимает элементы в порядке
        // front-to-back: первый элемент в слайсе рисуется поверх всех
        // остальных. Поэтому вставляем курсор в начало вектора.
        let cursor_elements = cursor_renderer.elements(
            renderer,
            &mut state.cursor_status,
            state.pointer_location,
            1.0,
        );
        let cursor_count = cursor_elements.len();
        elements.splice(0..0, cursor_elements);

        // Screencopy до DRM-рендера: offscreen-захват в своей текстуре, курсор
        // (первые cursor_count элементов) — только для overlay_cursor.
        // Вызывается на каждом рендере, чтобы copy_with_damage видел все изменения.
        screencopy.fulfill_pending(
            renderer,
            &state.screencopy_state,
            output,
            &elements,
            cursor_count,
            state.background_color(),
        );

        let mut comp = compositor.borrow_mut();
        let frame_flags = if state.fullscreen_window().is_some() {
            // Полноэкранное окно: его буфер — прямо на основную плоскость при
            // любом формате swapchain (игры отдают XR24, swapchain — с
            // альфой). Без overlay-плоскостей: на них дисплей сам смешивает
            // альфу буфера с тем, что под ним, а у игр в альфе мусор.
            FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT
                | FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY
                | FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT
        } else if state.layout_mode == crate::state::LayoutMode::Physics {
            // Без primary/overlay scanout: иначе DRM кладёт неповёрнутый буфер
            // на плоскость рядом с GLES-поворотом → «копия» окна на кадр.
            // Cursor-плоскость безопасна: на неё попадают только Kind::Cursor.
            FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT
        } else {
            FrameFlags::DEFAULT
        };
        match comp.render_frame::<_, _>(
            renderer,
            &elements,
            state.background_color(),
            frame_flags,
        ) {
            Ok(result) => {
                let scanout = matches!(
                    result.primary_element,
                    smithay::backend::drm::compositor::PrimaryPlaneElement::Element(_)
                );
                // Кадр без изменений DRM не показывает и vblank не пришлёт —
                // следующий кадр разбудит таймер.
                if !result.is_empty && comp.queue_frame(()).is_ok() {
                    *pending_since = Some(Instant::now());
                    if let Some(stats) = stats.as_mut() {
                        stats.frame(scanout, started.elapsed(), elements.len());
                    }
                }
                state.space.refresh();
                let now = state.clock.now();
                crate::render::send_frames(state, output, now);
                crate::cursor::send_frames(&state.cursor_status, output, now);

                // Frame rendered — clear the damage flag so we don't render
                // again until the next real change.
                state.needs_render = false;
            }
            Err(e) => eprintln!("[DRM] ошибка рендера: {e:?}"),
        }
    }
}

/// Период рендер-таймера по частоте режима (в мГц): 1/refresh, но не чаще
/// 4 мс; без данных о частоте — ~60 Гц.
fn frame_interval(refresh_mhz: i32) -> Duration {
    if refresh_mhz <= 0 {
        return Duration::from_millis(16);
    }
    Duration::from_micros(1_000_000_000 / refresh_mhz as u64).max(Duration::from_millis(4))
}

#[cfg(test)]
mod tests {
    use super::frame_interval;
    use std::time::Duration;

    #[test]
    fn frame_interval_follows_refresh() {
        assert_eq!(frame_interval(60_000), Duration::from_micros(16_666));
        assert_eq!(frame_interval(59_940), Duration::from_micros(16_683));
        assert_eq!(frame_interval(144_000), Duration::from_micros(6_944));
        assert_eq!(frame_interval(500_000), Duration::from_millis(4));
        assert_eq!(frame_interval(0), Duration::from_millis(16));
        assert_eq!(frame_interval(-1), Duration::from_millis(16));
    }
}
