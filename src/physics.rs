//! 2D физический движок (rapier2d) для "физического" режима WM.
//!
//! Окна становятся динамическими твёрдыми телами (`RigidBodyType::Dynamic`)
//! с прямоугольными коллайдерами. Гравитация тянет их вниз, статический
//! «пол» ловит их снизу. Координаты rapier = логические пиксели Smithay
//! (1.0 = 1 px), гравитация подобрана визуально для экрана (а не реалистичная
//! -9.81, которая на экране выглядит как невесомость).
//!
//! Это тонкая обёртка над `rapier2d::pipeline::PhysicsWorld` (появился в
//! rapier v0.34) — он сам хранит pipeline/islands/broad_phase/... и экспонирует
//! `insert` / `step` / `bodies[handle]`, так что нам не нужно таскать
//! 10 аргументов в каждый `step()`.

use rapier2d::math::Real;
use rapier2d::prelude::*;

use crate::config::PhysicsConfig;

pub struct WindowPhysics {
    world: PhysicsWorld,
    /// Копия physics-секции конфига (density/damping/drag и т.д.).
    cfg: PhysicsConfig,
}

impl WindowPhysics {
    /// Создаёт мир с гравитацией и полом из [`PhysicsConfig`].
    ///
    /// Гравитация в px/s² (не реалистичные 9.81 — на экране это выглядело бы
    /// как невесомость). Положительный знак: Logical/Smithay Y растёт вниз.
    pub fn new(cfg: PhysicsConfig) -> Self {
        let mut world = PhysicsWorld::new();
        world.gravity = Vector::new(0.0, cfg.gravity_y);
        // Пиксельный масштаб и усиленный solver уменьшают взаимное проникновение.
        world.integration_parameters.length_unit = 100.0;
        world.integration_parameters.num_solver_iterations = 12;
        world.integration_parameters.num_internal_pgs_iterations = 2;
        world
            .integration_parameters
            .num_internal_stabilization_iterations = 4;
        world.integration_parameters.max_ccd_substeps = 8;

        // Статический пол. fixed-тело не двигается под гравитацией и служит
        // бесконечной горизонтальной плоскостью, на которую падают окна.
        world.insert(
            RigidBodyBuilder::fixed().translation(Vector::new(0.0, cfg.floor_y + cfg.floor_half_h)),
            ColliderBuilder::cuboid(cfg.floor_half_w, cfg.floor_half_h),
        );

        Self { world, cfg }
    }

    /// Спавнит динамическое окно заданного размера (`w`×`h`, логические px)
    /// в мировой точке `(x, y)`. `x`/`y` — координаты центра тела в rapier.
    /// Возвращает хендл тела для последующего трекинга трансформы.
    ///
    /// `cuboid` принимает half-extents, поэтому делим размеры пополам.
    /// Небольшой `linear_damping` гасит горизонтальное скольжение; окна не
    /// должны кататься по полу вечно.
    pub fn spawn_window(&mut self, x: Real, y: Real, w: Real, h: Real) -> RigidBodyHandle {
        // Масса ∝ площади окна (density × w × h): большие окна тяжелее и
        // инертнее, маленькие легче опрокидываются и крутятся.
        let (body, _collider) = self.world.insert(
            RigidBodyBuilder::dynamic()
                .translation(Vector::new(x, y))
                // Drag может разгонять окно достаточно быстро для tunneling.
                // CCD проверяет весь путь тела между физическими шагами.
                .ccd_enabled(true)
                .linear_damping(self.cfg.linear_damping)
                .angular_damping(self.cfg.angular_damping),
            ColliderBuilder::cuboid(w * 0.5, h * 0.5)
                .density(self.cfg.density)
                .friction(self.cfg.friction)
                .restitution(self.cfg.restitution),
        );
        body
    }

    /// Задаёт угловую скорость (рад/с). Используется для удержания Super+A/D:
    /// solver интегрирует поворот вместе со столкновениями (без телепорта угла).
    pub fn set_angular_velocity(&mut self, handle: RigidBodyHandle, omega: Real) {
        if let Some(body) = self.world.bodies.get_mut(handle) {
            body.set_angvel(omega, true);
        }
    }

    /// Блокирует вращение тела. Для X11-окон: XWayland не умеет повёрнутую
    /// геометрию, глобальные координаты кликов (Steam/CEF) съезжают.
    pub fn lock_rotations(&mut self, handle: RigidBodyHandle) {
        if let Some(body) = self.world.bodies.get_mut(handle) {
            body.lock_rotations(true, true);
        }
    }

    /// Удаляет тело окна (и его коллайдер). Вызывается при закрытии окна
    /// или при выходе из физического режима.
    pub fn remove_window(&mut self, handle: RigidBodyHandle) {
        self.world.remove_body(handle);
    }

    /// Обновляет размер коллайдера тела, если окно изменило свою геометрию.
    /// Находит первый коллайдер, прикреплённый к телу, и заменяет его shape
    /// на cuboid с новыми half-extents.
    pub fn update_collider_size(&mut self, handle: RigidBodyHandle, w: Real, h: Real) {
        let collider_handles: Vec<ColliderHandle> = {
            let Some(body) = self.world.bodies.get(handle) else {
                return;
            };
            body.colliders().to_vec()
        };
        if let Some(&col_handle) = collider_handles.first() {
            if let Some(collider) = self.world.colliders.get_mut(col_handle) {
                collider.set_shape(SharedShape::cuboid(w * 0.5, h * 0.5));
            }
        }
    }

    /// Продвигает симуляцию на один шаг (`integration_parameters.dt`).
    /// Возвращает время, затраченное на `world.step()`, для диагностики лагов.
    pub fn step(&mut self) -> std::time::Duration {
        let start = std::time::Instant::now();
        self.world.step();

        // Аварийная координата под полом. Если CCD/solver всё же пропустили
        // тело, возвращаем его над полом и гасим накопленную скорость.
        let rescue_y = self.cfg.floor_y + self.cfg.floor_half_h / 0.25;
        let escaped: Vec<RigidBodyHandle> = self
            .world
            .bodies
            .iter()
            .filter(|(_, body)| body.is_dynamic() && body.translation().y > rescue_y)
            .map(|(handle, _)| handle)
            .collect();
        for handle in escaped {
            let vertical_extent = self
                .world
                .bodies
                .get(handle)
                .and_then(|body| {
                    body.colliders()
                        .first()
                        .copied()
                        .map(|collider| (body.rotation().angle(), collider))
                })
                .and_then(|(angle, collider)| {
                    self.world
                        .colliders
                        .get(collider)
                        .and_then(|c| c.shape().as_cuboid())
                        .map(|cuboid| {
                            let half = cuboid.half_extents;
                            angle.sin().abs() / half.x.recip() + angle.cos().abs() / half.y.recip()
                        })
                })
                .unwrap_or(50.0);
            if let Some(body) = self.world.bodies.get_mut(handle) {
                let safe_y = self.cfg.floor_y - vertical_extent - 4.0;
                body.set_translation(Vector::new(body.translation().x, safe_y), true);
                body.set_linvel(Vector::new(0.0, 0.0), true);
                body.set_angvel(0.0, true);
            }
        }
        start.elapsed()
    }

    /// Жёстко удерживает всё повёрнутое окно внутри видимой области.
    /// Возвращает true, если тело пришлось вернуть за границу экрана.
    #[allow(dead_code)]
    pub fn constrain_to_rect(
        &mut self,
        handle: RigidBodyHandle,
        min_x: Real,
        min_y: Real,
        max_x: Real,
        max_y: Real,
    ) -> bool {
        let Some(body) = self.world.bodies.get(handle) else {
            return false;
        };
        let Some(&collider_handle) = body.colliders().first() else {
            return false;
        };
        let Some(collider) = self.world.colliders.get(collider_handle) else {
            return false;
        };
        let Some(cuboid) = collider.shape().as_cuboid() else {
            return false;
        };
        let angle = body.rotation().angle();
        let half = cuboid.half_extents;
        let extent_x = angle.cos().abs() / half.x.recip() + angle.sin().abs() / half.y.recip();
        let extent_y = angle.sin().abs() / half.x.recip() + angle.cos().abs() / half.y.recip();
        let pos = body.translation();
        let old_vel = body.linvel();

        let low_x = min_x + extent_x;
        let high_x = max_x - extent_x;
        let low_y = min_y + extent_y;
        let high_y = max_y - extent_y;
        let mut x = if low_x <= high_x {
            pos.x.clamp(low_x, high_x)
        } else {
            (min_x + max_x) / 2.0
        };
        let mut y = if low_y <= high_y {
            pos.y.clamp(low_y, high_y)
        } else {
            (min_y + max_y) / 2.0
        };
        if !x.is_finite() {
            x = pos.x;
        }
        if !y.is_finite() {
            y = pos.y;
        }
        if x == pos.x && y == pos.y {
            return false;
        }

        let mut vx = old_vel.x;
        let mut vy = old_vel.y;
        if x != pos.x && ((x <= low_x && vx < 0.0) || (x >= high_x && vx > 0.0)) {
            vx = 0.0;
        }
        if y != pos.y && ((y <= low_y && vy < 0.0) || (y >= high_y && vy > 0.0)) {
            vy = 0.0;
        }
        if let Some(body) = self.world.bodies.get_mut(handle) {
            body.set_translation(Vector::new(x, y), true);
            body.set_linvel(Vector::new(vx, vy), true);
        }
        true
    }
    /// Возвращает количество тел в мире.
    pub fn body_count(&self) -> usize {
        self.world.bodies.len()
    }

    /// Задаёт timestep. Обычно = 1/60. Rapier не принимает dt в `step()`
    /// напрямую — он фиксирован в `integration_parameters.dt`.
    pub fn set_dt(&mut self, dt: Real) {
        self.world.integration_parameters.dt = dt;
    }

    /// Возвращает трансформу тела: `(x, y, angle)`. `angle` — поворот тела
    /// вокруг центра в радианах. В Phase 1 угол пока не применяется к окну
    /// (окна Smithay — axis-aligned прямоугольники), но он доступен для
    /// будущей фазы деформации/поворота.
    pub fn body_transform(&self, handle: RigidBodyHandle) -> Option<(Real, Real, Real)> {
        let body = self.world.bodies.get(handle)?;
        let t = body.translation();
        Some((t.x, t.y, body.rotation().angle()))
    }

    /// Мгновенно перемещает тело окна в `(x, y)` (используется при спавне/
    /// начальной расстановке, когда тело ещё ни с чем не может сталкиваться
    /// сиюминутно). НЕ использовать во время drag — телепорт динамического
    /// тела между кадрами заставляет solver разрешать "мгновенное" внедрение
    /// в пол/соседние окна как жёсткий контакт, что даёт рывки/дёрганье даже
    /// при низкой нагрузке на CPU. Для drag используй `drag_to`.
    /// `wake_up = true` будит спящее тело, иначе оно не встанет под гравитацией.
    pub fn set_window_pos(&mut self, handle: RigidBodyHandle, x: Real, y: Real) {
        if let Some(body) = self.world.bodies.get_mut(handle) {
            body.set_translation(Vector::new(x, y), true);
        }
    }

    /// Двигает тело к целевой точке `(target_x, target_y)` за один таймстеп
    /// `dt`, выставляя скорость вместо телепорта. Solver честно интегрирует
    /// движение и корректно разрешает столкновения по пути, вместо того чтобы
    /// разрешать внезапное проникновение как жёсткий контакт (источник
    /// рывков при drag). Используется во время перетаскивания окна мышью.
    ///
    /// Скорость ограничена `MAX_DRAG_SPEED`: резкий скачок курсора между
    /// кадрами (дрожание руки, лаг input-события) иначе даёт огромную
    /// мгновенную скорость. Это было незаметно ПОКА тело управляется —
    /// solver просто разгонял его в нужную сторону. Но при отпускании кнопки
    /// эта неограниченная скорость оставалась на теле и при столкновении с
    /// полом/другими окнами solver был вынужден разрешать сильный удар за
    /// один шаг — это и есть лаг именно в момент release.
    pub fn drag_to(&mut self, handle: RigidBodyHandle, target_x: Real, target_y: Real, dt: Real) {
        // Жёсткий верхний предел дополняет CCD и не даёт drag создать
        // экстремальный импульс, способный продавить стопку окон.
        let max_speed = self.cfg.max_drag_speed;

        // Во время drag курсор может уйти под пол, поэтому одна только контактная
        // реакция solver'а недостаточна: каждый кадр мы снова задавали телу
        // скорость вниз. Ограничиваем целевой центр верхней гранью пола с учётом
        // текущего поворота прямоугольного коллайдера.
        let floor_limit = self.world.bodies.get(handle).and_then(|body| {
            let angle = body.rotation().angle();
            let collider = body
                .colliders()
                .first()
                .and_then(|collider_handle| self.world.colliders.get(*collider_handle))?;
            let cuboid = collider.shape().as_cuboid()?;
            let half = cuboid.half_extents;
            let vertical_extent = angle.sin().abs() * half.x + angle.cos().abs() * half.y;
            Some(self.cfg.floor_y - vertical_extent)
        });
        let target_y = floor_limit.map_or(target_y, |limit| target_y.min(limit));

        if let Some(body) = self.world.bodies.get_mut(handle) {
            let cur = body.translation();
            let mut vx = (target_x - cur.x) / dt;
            let mut vy = (target_y - cur.y) / dt;
            let speed = vx.hypot(vy);
            if speed > max_speed {
                let scale = max_speed / speed;
                vx = vx / scale.recip();
                vy = vy / scale.recip();
            }
            body.set_linvel(Vector::new(vx, vy), true);
        }
    }

    /// Придаёт окну импульс скорости (толчок). Используется при отпускании
    /// после drag — окно «улетает» с той скоростью, с которой его тащили.
    pub fn set_window_velocity(&mut self, handle: RigidBodyHandle, vx: Real, vy: Real) {
        if let Some(body) = self.world.bodies.get_mut(handle) {
            body.set_linvel(Vector::new(vx, vy), true);
        }
    }

    /// Истинно, если хотя бы одно тело всё ещё двигается (не спит). Рендер-
    /// цикл использует это, чтобы держать `needs_render` поднятым: когда все
    /// окна улеглись, можно снова заснуть и не тратить GPU.
    pub fn any_moving(&self) -> bool {
        self.world.bodies.iter().any(|(_, b)| b.is_moving())
    }
}
