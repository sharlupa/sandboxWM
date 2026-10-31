//! Сессия (концепт §3): сохранение и восстановление мира и окон.

use std::collections::HashMap;
use std::time::Instant;

use rapier2d::math::Vector;
use smithay::desktop::Window;
use smithay::utils::Rectangle;

use crate::config::BodyKind;
use crate::physics::{Anchor, LinkKind, Material, WindowPhysics};
use crate::state::{AppState, LayoutMode};

use super::{Piece, window_app_id, window_pid, window_title};

impl AppState {
    /// Снимок всех рабочих столов для сохранения: верхний уровень — первый
    /// стол, остальные — в `workspaces` (пустые столы в тайлинге не пишутся).
    pub fn session_snapshot(&mut self) -> crate::session::SessionData {
        let active_windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| !Self::is_override_redirect(w))
            .cloned()
            .collect();
        let mut blocks = Vec::new();
        for idx in 0..self.workspaces.len().max(1) {
            let windows = if idx == self.active_workspace {
                active_windows.clone()
            } else {
                self.workspaces[idx].windows.clone()
            };
            let mut block = self.with_workspace(idx, |s| s.workspace_snapshot(&windows));
            block.workspace = idx;
            blocks.push(block);
        }
        let mut top = blocks.remove(0);
        top.active_workspace = self.active_workspace;
        top.workspaces = blocks
            .into_iter()
            .filter(|b| !b.windows.is_empty() || !b.lines.is_empty() || b.layout_mode == "physics")
            .collect();
        top
    }

    /// Снимок стола, чьи данные сейчас в полях `AppState`; `windows` — его
    /// окна в порядке стека.
    fn workspace_snapshot(&self, windows: &[Window]) -> crate::session::SessionData {
        use crate::session::{
            SavedAnchor, SavedJoint, SavedLine, SavedPiece, SavedWindow, SessionData,
        };
        let mut data = SessionData {
            layout_mode: match self.layout_mode {
                LayoutMode::Tiling => "tiling".into(),
                LayoutMode::Physics => "physics".into(),
            },
            camera: if self.layout_mode == LayoutMode::Physics && self.transition.is_none() {
                [self.camera_offset.0, self.camera_offset.1, self.camera_zoom]
            } else {
                [
                    self.saved_camera.0,
                    self.saved_camera.1,
                    self.saved_camera.2,
                ]
            },
            ..Default::default()
        };
        // Стол ни разу не был в физике: мира нет, но окна (для перезапуска
        // приложений и их стола) сохраняются — с пустым временным миром.
        let empty;
        let phys = match self.physics.as_ref() {
            Some(phys) => {
                data.floor_y = Some(phys.floor_y());
                phys
            }
            None => {
                data.layout_mode = "tiling".into();
                empty = WindowPhysics::new(&self.config, self.output_size().h as f32);
                &empty
            }
        };
        data.gravity_mode = if phys.zero_gravity() {
            "zero"
        } else {
            "normal"
        }
        .into();

        // Окна в порядке стека; запоминаем индекс каждого куска для связей.
        let mut piece_index: HashMap<u32, (usize, usize)> = HashMap::new();
        for window in windows {
            let pieces: Vec<&Piece> = self.pieces_of(window).collect();
            let size = window.geometry().size;
            let pid = window_pid(&self.display_handle, window);
            let mut saved = SavedWindow {
                app_id: window_app_id(window),
                title: window_title(window),
                x11: window.x11_surface().is_some(),
                command: pid
                    .and_then(crate::session::process_command)
                    .unwrap_or_default(),
                cwd: pid.and_then(crate::session::process_cwd),
                pid: pid.unwrap_or(0),
                size: [size.w, size.h],
                weight: pieces.first().map_or(1.0, |p| phys.piece_weight(p.id)),
                softness: pieces
                    .iter()
                    .find_map(|p| self.soft.get(&p.id))
                    .map_or(0.0, |s| s.params.softness.max(0.01)),
                body: self.window_body(window),
                ..Default::default()
            };
            for (i, p) in pieces.iter().enumerate() {
                let Some((c, angle)) = phys.piece_transform(p.id) else {
                    continue;
                };
                piece_index.insert(p.id, (data.windows.len(), i));
                saved.pieces.push(SavedPiece {
                    rect: [p.rect.loc.x, p.rect.loc.y, p.rect.size.w, p.rect.size.h],
                    shape: p.shape.clone(),
                    center: [c.x, c.y],
                    angle,
                    floating: phys.is_floating(p.id),
                });
            }
            if saved.pieces.is_empty() {
                // Окно ещё не было в физике: центр — его место в тайлинге.
                let geo = self.space.element_geometry(window).unwrap_or_default();
                saved.pieces.push(SavedPiece {
                    rect: [0.0, 0.0, size.w as f64, size.h as f64],
                    center: [
                        geo.loc.x as f32 + geo.size.w as f32 * 0.5,
                        geo.loc.y as f32 + geo.size.h as f32 * 0.5,
                    ],
                    ..Default::default()
                });
            }
            data.windows.push(saved);
        }
        for line in phys.line_views() {
            data.lines.push(SavedLine {
                material: line.material.name().into(),
                points: line.points.iter().map(|p| [p.x, p.y]).collect(),
                pinned: if line.material == Material::Rope {
                    line.pinned.to_vec()
                } else {
                    Vec::new()
                },
            });
        }
        for link in phys.link_views() {
            let (Some(a), Some(b)) = (
                piece_index.get(&link.a.piece),
                piece_index.get(&link.b.piece),
            ) else {
                continue;
            };
            data.joints.push(SavedJoint {
                kind: link.kind.name().into(),
                a: SavedAnchor {
                    window: a.0,
                    piece: a.1,
                    local: [link.a.local.x, link.a.local.y],
                },
                b: SavedAnchor {
                    window: b.0,
                    piece: b.1,
                    local: [link.b.local.x, link.b.local.y],
                },
            });
        }
        data
    }

    fn session_file(&self) -> (std::path::PathBuf, crate::session::SessionFormat) {
        let format = crate::session::SessionFormat::from_config(&self.config.session.format);
        (
            crate::session::session_path(&self.config.session.save_path, format),
            format,
        )
    }

    /// Сохраняет сессию (выход, автосохранение, Super+F5).
    /// Сохранить сессию сейчас (выход, горячая клавиша) — из главного потока.
    pub fn save_session(&mut self) {
        if !self.config.session.enabled {
            return;
        }
        let data = self.session_snapshot();
        let (path, format) = self.session_file();
        if let Some(a) = self.autosave.as_mut() {
            a.finish();
        }
        let written = crate::session::encode(&data, format)
            .and_then(|text| crate::session::write(&path, format, &text).map(|()| text));
        match written {
            Ok(text) => {
                log_saved(&path, &data);
                if let Some(a) = self.autosave.as_mut() {
                    a.saved(text, Instant::now());
                }
            }
            Err(e) => eprintln!("[session] не удалось сохранить {}: {e}", path.display()),
        }
    }

    /// Автосохранение по таймеру: снимок и текст — здесь, запись на диск — в
    /// фоне и только если сессия изменилась.
    fn autosave_session(&mut self) {
        let data = self.session_snapshot();
        let (path, format) = self.session_file();
        let text = match crate::session::encode(&data, format) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("[session] не удалось сохранить {}: {e}", path.display());
                return;
            }
        };
        let Some(autosave) = self.autosave.as_mut() else {
            return;
        };
        if autosave.write_in_background(path.clone(), format, text) {
            log_saved(&path, &data);
        }
    }

    pub fn maybe_autosave(&mut self, now: Instant) {
        self.maybe_relaunch(now);
        if self.autosave.as_mut().map_or(false, |a| a.due(now)) {
            self.autosave_session();
        }
        if let Some(r) = self.restorer.as_mut() {
            r.expire(now);
            if !r.is_pending() {
                self.restorer = None;
                self.pending_joints.clear();
            }
        }
    }

    /// Вызывается, когда у бэкенда появился выход: загрузка сессии,
    /// линии/связи, режим, перезапуск приложений.
    pub fn on_output_ready(&mut self) {
        if self.session_loaded {
            return;
        }
        self.session_loaded = true;
        let cfg = self.config.session.clone();
        if !cfg.enabled {
            return;
        }
        if cfg.autosave && cfg.autosave_interval_secs > 0 {
            self.autosave = Some(crate::session::Autosave::new(
                std::time::Duration::from_secs(cfg.autosave_interval_secs as u64),
                Instant::now(),
            ));
        }
        let (path, format) = self.session_file();
        let Some(data) = crate::session::load(&path, format) else {
            return;
        };
        let active = data.active_workspace;
        let top_floor = data.floor_y;
        let last = self.workspaces.len().saturating_sub(1);
        let mut blocks = data.into_workspace_blocks();
        for block in &mut blocks {
            // Столов в конфиге стало меньше — лишние уходят на последний.
            block.workspace = block.workspace.min(last);
        }
        let (merged, workspace_of) = crate::session::SessionData::merge_workspace_blocks(&blocks);
        println!(
            "[session] загружено: {} ({} окон, {} рабочих столов)",
            path.display(),
            merged.windows.len(),
            blocks.len()
        );
        for block in &blocks {
            self.with_workspace(block.workspace, |s| s.restore_workspace_world(block));
        }
        self.restored_floor_y = top_floor;
        self.pending_joints = merged.joints.clone();
        self.restore_workspaces = workspace_of;
        self.restorer = Some(crate::session::SessionRestorer::new(
            &merged,
            Instant::now(),
            std::time::Duration::from_secs(cfg.restore_timeout_secs.max(1) as u64),
        ));
        self.restored_windows.clear();
        if active != self.active_workspace && active < self.workspaces.len() {
            self.switch_workspace(active);
        }
        if cfg.restore_apps {
            // Запуск откладывается до готовности XWayland (DISPLAY), иначе
            // восстановленные X11-приложения (Steam) стартуют без X-сервера.
            self.pending_relaunch = crate::session::relaunch_commands(&merged);
            self.relaunch_deadline = Some(Instant::now() + std::time::Duration::from_secs(5));
        }
    }

    /// Мир стола из сессии: пол, камера, линии, гравитация, режим. Данные
    /// стола уже в полях `AppState` (см. `with_workspace`).
    fn restore_workspace_world(&mut self, block: &crate::session::SessionData) {
        self.restored_floor_y = block.floor_y;
        self.saved_camera = (
            block.camera[0],
            block.camera[1],
            block.camera[2].clamp(self.config.physics.zoom_min, self.config.physics.zoom_max),
        );
        let phys = self.ensure_physics();
        for line in &block.lines {
            let Some(material) = Material::from_name(&line.material) else {
                continue;
            };
            let pts: Vec<Vector> = line
                .points
                .iter()
                .map(|p| Vector::new(p[0], p[1]))
                .collect();
            let pinned = (line.pinned.len() == 2).then(|| [line.pinned[0], line.pinned[1]]);
            phys.add_line(material, &pts, pinned);
        }
        if block.gravity_mode == "zero" {
            phys.set_zero_gravity(true);
        }
        if block.layout_mode != "physics" {
            return;
        }
        if block.workspace == self.active_workspace {
            self.enter_physics_mode();
        } else {
            // Скрытый стол: окон в Space нет, камера ставится сразу.
            self.layout_mode = LayoutMode::Physics;
            self.camera_offset = (self.saved_camera.0, self.saved_camera.1);
            self.target_camera_offset = self.camera_offset;
            self.camera_zoom = self.saved_camera.2;
        }
    }

    /// Запускает приложения из сессии, как только XWayland готов (или через
    /// 5 секунд без него).
    fn maybe_relaunch(&mut self, now: Instant) {
        let Some(deadline) = self.relaunch_deadline else {
            return;
        };
        if self.xdisplay.is_none() && now < deadline {
            return;
        }
        self.relaunch_deadline = None;
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        for (argv, cwd) in std::mem::take(&mut self.pending_relaunch) {
            if let Err(e) = crate::session::spawn(&argv, cwd.as_deref(), &display) {
                eprintln!("[session] не удалось запустить {argv:?}: {e}");
            }
        }
    }

    /// Новое окно — одно из сохранённых? Тогда куски встают на старые места.
    pub(super) fn try_restore_window(&mut self, window: &Window) -> bool {
        let Some(restorer) = self.restorer.as_mut() else {
            return false;
        };
        let pid = window_pid(&self.display_handle, window);
        let command = pid.and_then(crate::session::process_command);
        let Some((index, saved)) = restorer.claim(
            &window_app_id(window),
            &window_title(window),
            window.x11_surface().is_some(),
            command.as_deref(),
        ) else {
            return false;
        };
        // Окно было на другом рабочем столе — уезжает туда, тела создаются
        // в мире того стола.
        let ws = self
            .restore_workspaces
            .get(index)
            .copied()
            .filter(|ws| *ws < self.workspaces.len())
            .unwrap_or(self.active_workspace);
        let ids = if ws != self.active_workspace {
            self.move_window_to_workspace(window, ws);
            self.with_workspace(ws, |s| s.restore_pieces(window, &saved, false))
        } else {
            self.restore_pieces(window, &saved, true)
        };
        if ids.is_empty() {
            // Перенесённое окно уже не на этом столе: тело ему создаст
            // показ его стола.
            return ws != self.active_workspace;
        }
        println!("[session] восстановлено окно {:?}", saved.app_id);
        self.restored_windows.insert(index, (ws, ids));
        self.restore_pending_joints();
        true
    }

    /// Куски окна на сохранённых местах (данные стола — в полях `AppState`;
    /// `active` — это активный стол, и его анимация перехода — его).
    fn restore_pieces(
        &mut self,
        window: &Window,
        saved: &crate::session::SavedWindow,
        active: bool,
    ) -> Vec<u32> {
        let size = window.geometry().size;
        let (sx, sy) = (
            if saved.size[0] > 0 {
                size.w as f64 / saved.size[0] as f64
            } else {
                1.0
            },
            if saved.size[1] > 0 {
                size.h as f64 / saved.size[1] as f64
            } else {
                1.0
            },
        );
        let mut ids = Vec::new();
        for p in &saved.pieces {
            if !(p.center[0].is_finite() && p.center[1].is_finite()) {
                ids.clear();
                break;
            }
            let rect = Rectangle::new(
                (p.rect[0] * sx, p.rect[1] * sy).into(),
                ((p.rect[2] * sx).max(1.0), (p.rect[3] * sy).max(1.0)).into(),
            );
            let shape = p
                .shape
                .as_ref()
                .map(|poly| poly.iter().map(|v| [v[0] * sx, v[1] * sy]).collect());
            let id = self.spawn_piece(
                window,
                rect,
                shape,
                Vector::new(p.center[0], p.center[1]),
                p.angle,
                saved.weight,
            );
            if p.floating {
                if let Some(phys) = self.physics.as_mut() {
                    phys.set_floating(id, true);
                }
            }
            if self.layout_mode == LayoutMode::Tiling || (active && self.transition.is_some()) {
                if let Some(phys) = self.physics.as_mut() {
                    phys.set_piece_enabled(id, false);
                }
            }
            ids.push(id);
        }
        // Старые сессии знают только мягкость: больше 0 — мягкое окно.
        let body = match saved.body {
            BodyKind::Rigid if saved.softness > 0.0 => BodyKind::Soft,
            body => body,
        };
        if body != BodyKind::Rigid {
            let mut params = self.soft_params(window, body);
            if body == BodyKind::Soft && saved.softness > 0.0 {
                params.softness = saved.softness;
            }
            for &id in &ids {
                self.make_soft(id, body, params);
            }
        }
        ids
    }

    /// Связи, оба конца которых уже восстановлены.
    fn restore_pending_joints(&mut self) {
        let mut rest = Vec::new();
        for joint in std::mem::take(&mut self.pending_joints) {
            let end = |a: &crate::session::SavedAnchor| {
                self.restored_windows
                    .get(&a.window)
                    .and_then(|(ws, ids)| Some((*ws, *ids.get(a.piece)?)))
            };
            let (a, b) = (end(&joint.a), end(&joint.b));
            match (a, b, LinkKind::from_name(&joint.kind)) {
                // Связь между окнами разных столов невозможна — отбрасываем.
                (Some((wa, _)), Some((wb, _)), Some(_)) if wa != wb => {}
                (Some((ws, a)), Some((_, b)), Some(kind)) => {
                    self.with_workspace(ws, |s| {
                        let Some(phys) = s.physics.as_mut() else {
                            return;
                        };
                        phys.add_link(
                            kind,
                            Anchor {
                                piece: a,
                                local: Vector::new(joint.a.local[0], joint.a.local[1]),
                            },
                            Anchor {
                                piece: b,
                                local: Vector::new(joint.b.local[0], joint.b.local[1]),
                            },
                        );
                    });
                }
                (_, _, Some(_)) => rest.push(joint),
                _ => {}
            }
        }
        self.pending_joints = rest;
    }

    /// Восстановить место окна из сессии, не включая физику (тайлинг).
    pub fn restore_window_for_later(&mut self, window: &Window) {
        // Одна попытка на окно: сопоставление читает /proc, а видеоплеер
        // коммитит по 60 раз в секунду.
        if self.restore_tried.insert(window.clone()) {
            self.try_restore_window(window);
        }
    }
}

/// Что сохранили — в лог.
fn log_saved(path: &std::path::Path, data: &crate::session::SessionData) {
    let all = || std::iter::once(data).chain(data.workspaces.iter());
    println!(
        "[session] сохранено: {} ({} окон, {} линий, {} связей, {} рабочих столов)",
        path.display(),
        all().map(|d| d.windows.len()).sum::<usize>(),
        all().map(|d| d.lines.len()).sum::<usize>(),
        all().map(|d| d.joints.len()).sum::<usize>(),
        all().count()
    );
}
