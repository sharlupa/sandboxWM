/// DRM/KMS + libinput backend — запуск sandboxWM прямо из TTY.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::{
                solid::{SolidColorBuffer, SolidColorRenderElement},
                Kind,
            },
            gles::GlesRenderer,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{timer::Timer, EventLoop},
        drm::control::{connector, Device as ControlDevice, ModeTypeFlags},
        wayland_server::DisplayHandle,
    },
    utils::{DeviceFd, Transform},
};
use smithay::reexports::rustix;
use crate::input::process_libinput_event;
use crate::render::CustomRenderElements;
use crate::state::AppState;

pub fn run_tty(
    event_loop: &mut EventLoop<AppState>,
    display_handle: DisplayHandle,
    state: &mut AppState,
) -> Result<(), Box<dyn std::error::Error>> {
    let loop_handle = event_loop.handle();

    // 1. libseat сессия
    let (session, notifier) = LibSeatSession::new()
        .map_err(|e| format!("libseat: {e}"))?;
    let seat_name = session.seat();
    println!("=====> Сессия: {seat_name}");
    state.session = Some(session.clone());



    // 2. GPU
    let gpu_path = primary_gpu(&seat_name)?
        .ok_or_else(|| "Видеокарта не найдена".to_string())?;
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
            let pref = if m.mode_type().contains(ModeTypeFlags::PREFERRED) { 1_000_000u32 } else { 0 };
            pref + m.size().0 as u32 * m.size().1 as u32
        })
        .copied()
        .ok_or_else(|| "Нет режимов монитора".to_string())?;

    println!("=====> Режим: {}x{}@{}Hz",
        drm_mode.size().0, drm_mode.size().1, drm_mode.vrefresh());

    // Энкодер + CRTC
    let encoder = connector
        .current_encoder()
        .and_then(|h| drm.get_encoder(h).ok())
        .or_else(|| {
            connector.encoders().iter()
                .find_map(|&h| drm.get_encoder(h).ok())
        })
        .ok_or_else(|| "Нет энкодера".to_string())?;

    let crtc = res
        .filter_crtcs(encoder.possible_crtcs())
        .into_iter()
        .next()
        .ok_or_else(|| "Нет CRTC".to_string())?;

    // 6. smithay Output
    let (w, h) = drm_mode.size();
    let smithay_mode = Mode {
        size: (w as i32, h as i32).into(),
        refresh: drm_mode.vrefresh() as i32 * 1000,
    };
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
        Some(smithay_mode), Some(Transform::Normal), None, Some((0, 0).into()),
    );
    output.set_preferred(smithay_mode);
    state.space.map_output(&output, (0, 0));

    // 7. DRM поверхность + DrmCompositor
    let drm_surface = drm.create_surface(crtc, drm_mode, &[connector.handle()])?;
    let drm_node = DrmNode::from_path(&gpu_path).ok();
    let exporter   = GbmFramebufferExporter::new(gbm.clone(), drm_node);
    let allocator  = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
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
    use smithay::backend::renderer::{ImportEgl, ImportDma};
    match renderer.bind_wl_display(&display_handle) {
        Ok(()) => println!("=====> EGL hardware-acceleration enabled"),
        Err(e) => eprintln!("[DRM] EGL bind_wl_display failed: {e:?}"),
    }

    let dmabuf_formats = renderer.dmabuf_formats();
    // The GBM device wraps the same DRM node we render on; its dev_id is the
    // main device clients should target when allocating buffers for us.
    let main_device = DrmNode::from_path(&gpu_path)
        .map(|n| n.dev_id())
        .unwrap_or_else(|_| drm_fd.dev_id().unwrap_or(0));
    let default_feedback =
        smithay::wayland::dmabuf::DmabufFeedbackBuilder::new(main_device, dmabuf_formats)
            .build()
            .expect("failed to build dmabuf feedback");
    let dmabuf_global = state.dmabuf_state
        .create_global_with_default_feedback::<AppState>(&display_handle, &default_feedback);
    state.dmabuf_global = Some(dmabuf_global);
    println!("=====> DMA-BUF (zero-copy) global создан");

    // 7.4. libinput
    use smithay::reexports::input as libinput_raw;
    let mut libinput_ctx =
        libinput_raw::Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput_ctx.udev_assign_seat(&seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput_ctx.clone());
    loop_handle.insert_source(libinput_backend, |event, _, state| {
        process_libinput_event(state, event);
    })?;

    // 7.5. Обработка событий сессии (VT-переключения)
    let compositor_session = compositor.clone();
    let mut libinput_session = libinput_ctx;
    loop_handle.insert_source(notifier, move |event, _, state| match event {
        SessionEvent::ActivateSession => {
            state.session_paused = false;
            state.needs_render = true;
            if let Err(e) = libinput_session.resume() {
                eprintln!("[libinput] Ошибка при resume: {:?}", e);
            }
            if let Err(e) = compositor_session.borrow_mut().reset_state() {
                eprintln!("[DRM] Ошибка при сбросе состояния после активации сессии: {:?}", e);
            }
            println!("[session] активна");
        }
        SessionEvent::PauseSession => {
            state.session_paused = true;
            libinput_session.suspend();
            println!("[session] пауза");
        }
    })?;

    // 8. VBlank — обязательно подтверждаем каждый кадр
    let compositor_vb = compositor.clone();
    loop_handle.insert_source(drm_notifier, move |event, _, _| {
        if let DrmEvent::VBlank(_) = event {
            compositor_vb.borrow_mut().frame_submitted().ok();
        }
    })?;


    // 10. udev hotplug
    let udev_backend = UdevBackend::new(&seat_name)?;
    loop_handle.insert_source(udev_backend, |event, _, _| match event {
        UdevEvent::Added { path, .. }    => println!("[udev] GPU добавлена: {path:?}"),
        UdevEvent::Changed { .. }        => {}
        UdevEvent::Removed { device_id } => println!("[udev] GPU удалена: {device_id}"),
    })?;

    // Курсор — простой белый квадрат 12x12 (рисуется программно как render-element,
    // т.к. в Smithay 0.7 у DrmCompositor нет API для hardware cursor plane).
    let cursor_buf = SolidColorBuffer::new((12, 12), [1.0f32, 1.0, 1.0, 1.0]);

    // 11. Рендер-таймер ~60 fps. Renders only when something changed
    //     (`state.needs_render`); an idle desktop costs ~0 GPU/CPU instead of a
    //     forced 60fps redraw loop.
    let output_t     = output.clone();
    let compositor_t = compositor.clone();
    loop_handle.insert_source(
        Timer::from_duration(Duration::from_millis(16)),
        move |_, _, state| {
            if state.session_paused || !state.needs_render {
                return smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                    Duration::from_millis(16),
                );
            }

            // Process deferred layout (resize, etc.) once per frame.
            if state.layout_dirty {
                state.recalculate_layout();
                state.layout_dirty = false;
            }

            // Окна из space, маппим в общий enum с курсором.
            let mut elements: Vec<CustomRenderElements> = state
                .space
                .render_elements_for_output(&mut renderer, &output_t, 1.0)
                .unwrap_or_default()
                .into_iter()
                .map(CustomRenderElements::Space)
                .collect();

            // Программный курсор поверх окон. `render_frame` принимает элементы
            // в порядке front-to-back: первый элемент в слайсе рисуется поверх
            // всех остальных. Поэтому вставляем курсор в начало вектора.
            // `pointer_location` хранится в logical координатах; from_buffer
            // принимает physical-позицию, поэтому переводим через to_physical(1.0).
            let cursor_loc = state.pointer_location.to_physical(1.0).to_i32_round();
            elements.insert(0, CustomRenderElements::Cursor(
                SolidColorRenderElement::from_buffer(
                    &cursor_buf,
                    cursor_loc,
                    1.0,
                    1.0,
                    Kind::Cursor,
                ),
            ));

            let mut comp = compositor_t.borrow_mut();
            match comp.render_frame::<_, _>(
                &mut renderer,
                &elements,
                [0.08, 0.08, 0.12, 1.0],
                FrameFlags::DEFAULT,
            ) {
                Ok(_result) => {
                    comp.queue_frame(()).ok();
                    state.space.refresh();
                    let now = state.clock.now();
                    for window in state.space.elements() {
                        window.send_frame(
                            &output_t,
                            now,
                            Some(Duration::ZERO),
                            |_, _| Some(output_t.clone()),
                        );
                    }
                    // Frame submitted successfully — clear the damage flag so we
                    // don't render again until the next real change.
                    state.needs_render = false;
                }
                Err(e) => eprintln!("[DRM] ошибка рендера: {e:?}"),
            }

            smithay::reexports::calloop::timer::TimeoutAction::ToDuration(
                Duration::from_millis(16),
            )
        },
    )?;

    Ok(())
}
