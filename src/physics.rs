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

/// Мировая Y-координата пола. Окна падают сверху и ложатся на эту высоту.
/// Физический мир уходит в бесконечность во все стороны; пол — единственная
/// граница снизу.
const FLOOR_Y: Real = 2000.0;
/// Гравитация в px/s². Реалистичные 9.81 на экране выглядят как невесомость
/// (окна падают часами); 1500 даёт ощутимое, но не мгновенное падение.
/// ВАЖНО: положительный знак — Logical/Smithay Y растёт ВНИЗ, а пол лежит
/// на FLOOR_Y = +2000 (больший Y). Отрицательная гравитация тянула бы тела
/// к меньшим Y, то есть вверх по экрану — именно так проявлялся баг
/// "окна падают вверх".
const GRAVITY_Y: Real = 1500.0;
/// Полутолщина пола по X (он тянется от -FLOOR_HALF_W до +FLOOR_HALF_W).
/// Достаточно велика, чтобы окна не улетали за край при обычной работе.
const FLOOR_HALF_W: Real = 50_000.0;
/// Полутолщина пола по Y (rapier принимает half-extents в `cuboid`).
const FLOOR_HALF_H: Real = 100.0;

pub struct WindowPhysics {
    world: PhysicsWorld,
}

impl WindowPhysics {
    pub fn new() -> Self {
        let mut world = PhysicsWorld::new();
        world.gravity = Vector::new(0.0, GRAVITY_Y);

        // Статический пол. fixed-тело не двигается под гравитацией и служит
        // бесконечной горизонтальной плоскостью, на которую падают окна.
        world.insert(
            RigidBodyBuilder::fixed().translation(Vector::new(0.0, FLOOR_Y + FLOOR_HALF_H)),
            ColliderBuilder::cuboid(FLOOR_HALF_W, FLOOR_HALF_H),
        );

        Self { world }
    }

    /// Спавнит динамическое окно заданного размера (`w`×`h`, логические px)
    /// в мировой точке `(x, y)`. `x`/`y` — координаты центра тела в rapier.
    /// Возвращает хендл тела для последующего трекинга трансформы.
    ///
    /// `cuboid` принимает half-extents, поэтому делим размеры пополам.
    /// Небольшой `linear_damping` гасит горизонтальное скольжение; окна не
    /// должны кататься по полу вечно.
    pub fn spawn_window(&mut self, x: Real, y: Real, w: Real, h: Real) -> RigidBodyHandle {
        let (body, _collider) = self.world.insert(
            RigidBodyBuilder::dynamic()
                .translation(Vector::new(x, y))
                .linear_damping(2.0)
                .lock_rotations(),
            ColliderBuilder::cuboid(w * 0.5, h * 0.5).friction(0.7),
        );
        body
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
            let Some(body) = self.world.bodies.get(handle) else { return };
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
        start.elapsed()
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
        const MAX_DRAG_SPEED: Real = 4000.0; // px/s — быстрое, но не взрывное перетаскивание
        if let Some(body) = self.world.bodies.get_mut(handle) {
            let cur = body.translation();
            let mut vx = (target_x - cur.x) / dt;
            let mut vy = (target_y - cur.y) / dt;
            let speed = (vx * vx + vy * vy).sqrt();
            if speed > MAX_DRAG_SPEED {
                let scale = MAX_DRAG_SPEED / speed;
                vx *= scale;
                vy *= scale;
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
