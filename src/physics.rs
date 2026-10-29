//! 2D физический мир (rapier2d) для физического режима WM.
//!
//! Координаты rapier = логические пиксели Smithay (1.0 = 1 px), Y растёт вниз,
//! гравитация подобрана визуально (px/s²).
//!
//! Что живёт в мире:
//! - **куски окон** (`piece`): динамические тела-прямоугольники. Неразрезанное
//!   окно — один кусок; нож (концепт §7) делит окно на несколько кусков;
//! - **пол** — широкое статическое тело;
//! - **линии** (концепт §2): статические цепочки капсул из стали/клея/батута
//!   и динамические верёвки (цепь звеньев на шарнирах);
//! - **связи** между окнами (концепт §4): сварка, пружина, верёвка;
//! - **перетаскивание**: кинематическое тело-«курсор» и шарнир в точке
//!   захвата — окно висит на курсоре и раскачивается, а не телепортируется
//!   центром к указателю.
//!
//! Тип каждого коллайдера закодирован в его `user_data` (см. [`Tag`]); по нему
//! физические хуки отключают контакты верёвки с окнами, которые она связывает.

use std::collections::{HashMap, HashSet};

use rapier2d::math::{Pose, Real, Rotation, Vector};
use rapier2d::prelude::*;

use crate::config::{Config, GravityModesConfig, MaterialsConfig, PhysicsConfig};

/// Материал нарисованной линии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Material {
    Steel,
    Glue,
    Trampoline,
    Rope,
}

impl Material {
    pub const ALL: [Material; 4] = [
        Material::Steel,
        Material::Glue,
        Material::Trampoline,
        Material::Rope,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Material::Steel => "steel",
            Material::Glue => "glue",
            Material::Trampoline => "trampoline",
            Material::Rope => "rope",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|m| m.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn color(self, materials: &MaterialsConfig) -> [f32; 4] {
        let hex = match self {
            Material::Steel => &materials.steel.color,
            Material::Glue => &materials.glue.color,
            Material::Trampoline => &materials.trampoline.color,
            Material::Rope => &materials.rope.color,
        };
        crate::config::parse_color(hex)
    }

    pub fn thickness(self, materials: &MaterialsConfig) -> f32 {
        match self {
            Material::Steel => materials.steel.thickness,
            Material::Glue => materials.glue.thickness,
            Material::Trampoline => materials.trampoline.thickness,
            Material::Rope => materials.rope.thickness,
        }
        .max(1.0)
    }
}

/// Вид связи между окнами.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    Weld,
    Spring,
    Rope,
}

impl LinkKind {
    pub fn name(self) -> &'static str {
        match self {
            LinkKind::Weld => "weld",
            LinkKind::Spring => "spring",
            LinkKind::Rope => "rope",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [LinkKind::Weld, LinkKind::Spring, LinkKind::Rope]
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name.trim()))
    }
}

/// Точка крепления на куске окна (локальные координаты: начало в центре куска).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub piece: u32,
    pub local: Vector,
}

/// Что за объект у коллайдера (кодируется в `Collider::user_data`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    None,
    Floor,
    Piece(u32),
    Line(u32),
    /// Звено верёвки. `excluded` — куски, с которыми звено не сталкивается
    /// (окна, которые эта верёвка связывает/держит).
    Rope { excluded: [u32; 2] },
}

const KIND_SHIFT: u32 = 120;

impl Tag {
    fn encode(self) -> u128 {
        match self {
            Tag::None => 0,
            Tag::Floor => 1 << KIND_SHIFT,
            Tag::Piece(id) => (2 << KIND_SHIFT) | id as u128,
            Tag::Line(id) => (3 << KIND_SHIFT) | id as u128,
            Tag::Rope { excluded } => {
                (4 << KIND_SHIFT) | ((excluded[1] as u128) << 32) | excluded[0] as u128
            }
        }
    }

    fn decode(data: u128) -> Self {
        let low = data as u32;
        match data >> KIND_SHIFT {
            1 => Tag::Floor,
            2 => Tag::Piece(low),
            3 => Tag::Line(low),
            4 => Tag::Rope {
                excluded: [low, (data >> 32) as u32],
            },
            _ => Tag::None,
        }
    }
}

/// Хуки rapier: звенья верёвок не сталкиваются друг с другом и с окнами,
/// которые они связывают (концепт §4: «по умолчанию верёвка проходит сквозь
/// скреплённые окна»).
struct Hooks;

impl PhysicsHooks for Hooks {
    fn filter_contact_pair(&self, ctx: &PairFilterContext) -> Option<SolverFlags> {
        let t1 = Tag::decode(ctx.colliders[ctx.collider1].user_data);
        let t2 = Tag::decode(ctx.colliders[ctx.collider2].user_data);
        let excluded = |ex: [u32; 2], p: u32| p != 0 && ex.contains(&p);
        match (t1, t2) {
            (Tag::Rope { .. }, Tag::Rope { .. }) => None,
            (Tag::Rope { excluded: ex }, Tag::Piece(p))
            | (Tag::Piece(p), Tag::Rope { excluded: ex })
                if excluded(ex, p) =>
            {
                None
            }
            _ => {
                // Поведение по умолчанию: два нединамических тела не сталкиваются.
                let dynamic = |h: Option<RigidBodyHandle>| {
                    h.and_then(|h| ctx.bodies.get(h))
                        .map_or(false, |b| b.is_dynamic())
                };
                if dynamic(ctx.rigid_body1) || dynamic(ctx.rigid_body2) {
                    Some(SolverFlags::COMPUTE_IMPULSES)
                } else {
                    None
                }
            }
        }
    }
}

struct PieceBody {
    body: RigidBodyHandle,
    collider: ColliderHandle,
    half: Vector,
    floating: bool,
    /// Окно-кусок держит верёвка/клей и т.п. — для отрисовки не нужно, но
    /// нужно, чтобы корректно чистить связи при удалении.
    weight: f32,
}

/// Цепь звеньев (нарисованная верёвка или верёвка-связь).
struct RopeChain {
    segments: Vec<RigidBodyHandle>,
    half_len: f32,
    /// Шарниры между звеньями и крепления концов.
    joints: Vec<ImpulseJointHandle>,
    /// Фиксированные тела-якоря закреплённых концов.
    anchors: Vec<RigidBodyHandle>,
    pinned: [bool; 2],
    /// Окно, подвешенное на свободный конец (нарисованная верёвка), и
    /// шарниры подвеса.
    hook: Option<(u32, Vec<ImpulseJointHandle>)>,
}

enum LineKind {
    Static {
        body: RigidBodyHandle,
        colliders: Vec<ColliderHandle>,
        points: Vec<Vector>,
    },
    Rope(RopeChain),
}

struct Line {
    material: Material,
    kind: LineKind,
}

struct Link {
    kind: LinkKind,
    a: Anchor,
    b: Anchor,
    joints: Vec<ImpulseJointHandle>,
    rope: Option<RopeChain>,
    /// Порог разрушения (Δv px/s за шаг), 0 = неразрушимая.
    break_dv: f32,
}

struct GlueBond {
    piece: u32,
    line: u32,
    joint: ImpulseJointHandle,
}

struct Drag {
    piece: u32,
    cursor: RigidBodyHandle,
    joint: ImpulseJointHandle,
    target: Vector,
}

/// Снимок линии для рендера/сохранения.
pub struct LineView {
    pub id: u32,
    pub material: Material,
    pub points: Vec<Vector>,
    pub pinned: [bool; 2],
}

/// Снимок связи для рендера/сохранения.
pub struct LinkView {
    pub id: u32,
    pub kind: LinkKind,
    pub a: Anchor,
    pub b: Anchor,
    /// Мировые точки для отрисовки: концы (weld/spring) или цепь (rope).
    pub points: Vec<Vector>,
}

pub struct WindowPhysics {
    world: PhysicsWorld,
    cfg: PhysicsConfig,
    gravity_modes: GravityModesConfig,
    materials: MaterialsConfig,
    joints_cfg: crate::config::JointsConfig,
    floor_y: f32,
    zero_gravity: bool,
    next_id: u32,
    accumulator: f32,
    pieces: HashMap<u32, PieceBody>,
    lines: HashMap<u32, Line>,
    links: HashMap<u32, Link>,
    glue: Vec<GlueBond>,
    /// Кусок недавно оторван от клея — не приклеивать снова, пока не отлетит.
    glue_cooldown: HashMap<u32, f32>,
    drag: Option<Drag>,
    /// Удержание Super+A/D: кусок и угловая скорость.
    spin: Option<(u32, f32)>,
}

impl WindowPhysics {
    /// Создаёт мир с гравитацией и полом. `floor_y` — мировая Y верхней
    /// грани пола.
    pub fn new(config: &Config, floor_y: f32) -> Self {
        let cfg = config.physics.clone();
        let mut world = PhysicsWorld::new();
        world.gravity = Vector::new(0.0, cfg.gravity_y);
        world.integration_parameters.dt = cfg.physics_dt.clamp(1.0 / 480.0, 1.0 / 20.0);
        // Пиксельный масштаб и усиленный solver уменьшают взаимное проникновение.
        world.integration_parameters.length_unit = 100.0;
        world.integration_parameters.num_solver_iterations = 12;
        world.integration_parameters.num_internal_pgs_iterations = 2;
        world
            .integration_parameters
            .num_internal_stabilization_iterations = 4;
        world.integration_parameters.max_ccd_substeps = 8;

        world.insert(
            RigidBodyBuilder::fixed().translation(Vector::new(0.0, floor_y + cfg.floor_half_h)),
            ColliderBuilder::cuboid(cfg.floor_half_w, cfg.floor_half_h)
                .friction(cfg.friction)
                .user_data(Tag::Floor.encode()),
        );

        Self {
            world,
            cfg,
            gravity_modes: config.gravity_modes.clone(),
            materials: config.materials.clone(),
            joints_cfg: config.joints.clone(),
            floor_y,
            zero_gravity: false,
            next_id: 1,
            accumulator: 0.0,
            pieces: HashMap::new(),
            lines: HashMap::new(),
            links: HashMap::new(),
            glue: Vec::new(),
            glue_cooldown: HashMap::new(),
            drag: None,
            spin: None,
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    pub fn floor_y(&self) -> f32 {
        self.floor_y
    }

    pub fn dt(&self) -> f32 {
        self.world.integration_parameters.dt
    }

    // ── Симуляция ──────────────────────────────────────────────────

    /// Продвигает симуляцию на `elapsed` секунд фиксированными шагами
    /// (накопитель). Возвращает число выполненных шагов. Не больше 4 шагов
    /// за вызов, чтобы после фриза рендера мир не «догонял» секунды разом.
    pub fn advance(&mut self, elapsed: f32) -> u32 {
        let dt = self.dt();
        self.accumulator = (self.accumulator + elapsed.max(0.0)).min(dt * 4.0);
        let mut steps = 0;
        while self.accumulator >= dt * 0.999 {
            self.accumulator -= dt;
            self.step_once();
            steps += 1;
        }
        steps
    }

    fn step_once(&mut self) {
        let dt = self.dt();
        self.update_drag_target(dt);
        if let Some((piece, omega)) = self.spin {
            self.set_angvel_raw(piece, omega);
        }
        self.world.step_with_events(&Hooks, &());
        self.process_glue(dt);
        self.process_breaks();
        self.process_rope_hooks();
        self.rescue_fallen();
    }

    /// Аварийный возврат тел, провалившихся сквозь пол (CCD/solver не всесильны).
    fn rescue_fallen(&mut self) {
        let limit = self.floor_y + self.cfg.floor_half_h * 2.0;
        let mut escaped = Vec::new();
        for (handle, body) in self.world.bodies.iter() {
            if !body.is_dynamic() {
                continue;
            }
            let top = body
                .colliders()
                .iter()
                .filter_map(|c| self.world.colliders.get(*c))
                .map(|c| c.compute_aabb().mins.y)
                .fold(f32::INFINITY, f32::min);
            if top.is_finite() && top > limit {
                let bottom = body
                    .colliders()
                    .iter()
                    .filter_map(|c| self.world.colliders.get(*c))
                    .map(|c| c.compute_aabb().maxs.y)
                    .fold(f32::NEG_INFINITY, f32::max);
                escaped.push((handle, bottom));
            }
        }
        for (handle, bottom) in escaped {
            if let Some(body) = self.world.bodies.get_mut(handle) {
                let t = body.translation();
                let lift = bottom - self.floor_y + 4.0;
                body.set_translation(Vector::new(t.x, t.y - lift), true);
                body.set_linvel(Vector::ZERO, true);
                body.set_angvel(0.0, true);
            }
        }
    }

    /// Истинно, если хоть одно тело ещё движется (рендер держит кадры).
    pub fn any_moving(&self) -> bool {
        self.drag.is_some() || self.world.bodies.iter().any(|(_, b)| b.is_moving())
    }

    // ── Режимы гравитации ──────────────────────────────────────────

    /// Режим 4: невесомость для всех окон. Возврат — режим 1.
    pub fn set_zero_gravity(&mut self, zero: bool) {
        if self.zero_gravity == zero {
            return;
        }
        self.zero_gravity = zero;
        self.world.gravity = Vector::new(
            0.0,
            if zero {
                self.gravity_modes.weightless_gravity_y
            } else {
                self.cfg.gravity_y
            },
        );
        let damping = self.piece_damping();
        let handles: Vec<RigidBodyHandle> = self.pieces.values().map(|p| p.body).collect();
        for h in handles {
            if let Some(body) = self.world.bodies.get_mut(h) {
                body.set_linear_damping(damping);
                body.wake_up(true);
            }
        }
        self.world.wake_up_all(true);
    }

    pub fn zero_gravity(&self) -> bool {
        self.zero_gravity
    }

    fn piece_damping(&self) -> f32 {
        if self.zero_gravity {
            self.gravity_modes.weightless_linear_damping
        } else {
            self.cfg.linear_damping
        }
    }

    /// Режим 3: невесомость одного окна. Как в концепте, тело становится
    /// кинематическим: окно висит в воздухе, его не сдвигают ни удары других
    /// окон, ни подвешенные на нём грузы, но тащить мышью и крутить можно.
    pub fn set_floating(&mut self, piece: u32, floating: bool) {
        let Some(p) = self.pieces.get_mut(&piece) else {
            return;
        };
        p.floating = floating;
        if let Some(body) = self.world.bodies.get_mut(p.body) {
            if floating {
                body.set_body_type(RigidBodyType::KinematicPositionBased, true);
                body.set_linvel(Vector::ZERO, true);
                body.set_angvel(0.0, true);
            } else {
                body.set_body_type(RigidBodyType::Dynamic, true);
            }
        }
        self.world.wake_up_all(true);
    }

    pub fn is_floating(&self, piece: u32) -> bool {
        self.pieces.get(&piece).map_or(false, |p| p.floating)
    }

    // ── Куски окон ─────────────────────────────────────────────────

    /// Создаёт динамическое тело куска окна `w`×`h` с центром в `center`.
    /// Масса ∝ площади × `weight`.
    pub fn spawn_piece(
        &mut self,
        center: Vector,
        angle: f32,
        w: f32,
        h: f32,
        weight: f32,
        lock_rotation: bool,
    ) -> u32 {
        let id = self.alloc_id();
        let mut builder = RigidBodyBuilder::dynamic()
            .translation(center)
            .rotation(angle)
            // Drag/броски разгоняют окно — CCD не даёт пролететь сквозь пол.
            .ccd_enabled(true)
            .linear_damping(self.piece_damping())
            .angular_damping(self.cfg.angular_damping);
        if lock_rotation {
            builder = builder.lock_rotations();
        }
        let weight = if weight.is_finite() && weight > 0.0 {
            weight
        } else {
            1.0
        };
        let (body, collider) = self.world.insert(
            builder,
            ColliderBuilder::cuboid((w * 0.5).max(1.0), (h * 0.5).max(1.0))
                .density(self.cfg.density * weight)
                .friction(self.cfg.friction)
                .restitution(self.cfg.restitution)
                .user_data(Tag::Piece(id).encode()),
        );
        self.pieces.insert(
            id,
            PieceBody {
                body,
                collider,
                half: Vector::new(w * 0.5, h * 0.5),
                floating: false,
                weight,
            },
        );
        id
    }

    /// Удаляет кусок вместе со всеми его связями, клеем и креплениями.
    pub fn remove_piece(&mut self, piece: u32) {
        let Some(p) = self.pieces.remove(&piece) else {
            return;
        };
        if self.drag.as_ref().map_or(false, |d| d.piece == piece) {
            self.drag_end();
        }
        self.unlink_piece(piece);
        self.glue_cooldown.remove(&piece);
        self.world.remove_body(p.body);
    }

    /// Выключенный кусок не участвует в симуляции (окно в тайлинге), но
    /// сохраняет позу и связи — при возврате в физику всё продолжается.
    pub fn set_piece_enabled(&mut self, piece: u32, enabled: bool) {
        if let Some(body) = self
            .pieces
            .get(&piece)
            .and_then(|p| self.world.bodies.get_mut(p.body))
        {
            body.set_enabled(enabled);
            if enabled {
                body.set_linvel(Vector::ZERO, true);
                body.set_angvel(0.0, true);
                body.wake_up(true);
            }
        }
    }

    pub fn has_piece(&self, piece: u32) -> bool {
        self.pieces.contains_key(&piece)
    }

    /// `(центр, угол)` куска.
    pub fn piece_transform(&self, piece: u32) -> Option<(Vector, f32)> {
        let p = self.pieces.get(&piece)?;
        let body = self.world.bodies.get(p.body)?;
        Some((body.translation(), body.rotation().angle()))
    }

    /// `(линейная, угловая)` скорость куска.
    pub fn piece_velocity(&self, piece: u32) -> Option<(Vector, f32)> {
        let p = self.pieces.get(&piece)?;
        let body = self.world.bodies.get(p.body)?;
        Some((body.linvel(), body.angvel()))
    }

    pub fn set_piece_velocity(&mut self, piece: u32, linvel: Vector, angvel: f32) {
        if let Some(body) = self
            .pieces
            .get(&piece)
            .and_then(|p| self.world.bodies.get_mut(p.body))
        {
            body.set_linvel(linvel, true);
            body.set_angvel(angvel, true);
        }
    }

    /// Мгновенно ставит кусок (спавн/восстановление сессии, не для drag).
    pub fn set_piece_transform(&mut self, piece: u32, center: Vector, angle: f32) {
        if let Some(body) = self
            .pieces
            .get(&piece)
            .and_then(|p| self.world.bodies.get_mut(p.body))
        {
            body.set_position(Pose::new(center, angle), true);
        }
    }

    pub fn piece_half_extents(&self, piece: u32) -> Option<Vector> {
        self.pieces.get(&piece).map(|p| p.half)
    }

    /// Клиент поменял размер окна — коллайдер следует за ним.
    pub fn set_piece_size(&mut self, piece: u32, w: f32, h: f32) {
        let Some(p) = self.pieces.get_mut(&piece) else {
            return;
        };
        let half = Vector::new((w * 0.5).max(1.0), (h * 0.5).max(1.0));
        if (half - p.half).abs().max_element() < 0.25 {
            return;
        }
        p.half = half;
        if let Some(collider) = self.world.colliders.get_mut(p.collider) {
            collider.set_shape(SharedShape::cuboid(half.x, half.y));
        }
        if let Some(body) = self.world.bodies.get_mut(p.body) {
            body.wake_up(true);
        }
    }

    /// Форма коллайдера по выпуклой оболочке (деформированное мягкое окно).
    /// `None`/вырожденная оболочка → обратно к прямоугольнику.
    pub fn set_piece_hull(&mut self, piece: u32, hull: Option<&[Vector]>) {
        let Some(p) = self.pieces.get(&piece) else {
            return;
        };
        let shape = hull
            .filter(|pts| pts.len() >= 3)
            .and_then(SharedShape::convex_hull)
            .unwrap_or_else(|| SharedShape::cuboid(p.half.x, p.half.y));
        if let Some(collider) = self.world.colliders.get_mut(p.collider) {
            collider.set_shape(shape);
        }
    }

    /// Крутить кусок с угловой скоростью `omega` (рад/с), пока не снято.
    /// Скорость выставляется каждый шаг — solver интегрирует поворот вместе со
    /// столкновениями, без телепорта угла.
    pub fn set_spin(&mut self, spin: Option<(u32, f32)>) {
        if let Some((piece, _)) = self.spin.take() {
            if spin.map_or(true, |(p, _)| p != piece) {
                self.set_angvel_raw(piece, 0.0);
            }
        }
        self.spin = spin;
    }

    fn set_angvel_raw(&mut self, piece: u32, omega: f32) {
        if let Some(body) = self
            .pieces
            .get(&piece)
            .and_then(|p| self.world.bodies.get_mut(p.body))
        {
            if body.is_rotation_locked() {
                return;
            }
            if body.is_kinematic() {
                let next = body.rotation().angle() + omega * self.world.integration_parameters.dt;
                body.set_next_kinematic_rotation(Rotation::new(next));
            } else {
                body.set_angvel(omega, true);
            }
        }
    }

    pub fn piece_mass(&self, piece: u32) -> f32 {
        self.pieces
            .get(&piece)
            .and_then(|p| self.world.bodies.get(p.body))
            .map_or(1.0, |b| b.mass().max(1e-3))
    }

    pub fn piece_weight(&self, piece: u32) -> f32 {
        self.pieces.get(&piece).map_or(1.0, |p| p.weight)
    }

    // ── Перетаскивание мышью ───────────────────────────────────────

    /// Хватает кусок в мировой точке `world`: кинематическое тело-курсор
    /// и шарнир в точке захвата.
    pub fn drag_begin(&mut self, piece: u32, world: Vector) -> bool {
        self.drag_end();
        let Some(p) = self.pieces.get(&piece) else {
            return false;
        };
        let Some(body) = self.world.bodies.get(p.body) else {
            return false;
        };
        let local = body.position().inverse_transform_point(world);
        let cursor = self
            .world
            .insert_body(RigidBodyBuilder::kinematic_position_based().translation(world));
        let joint = self.world.insert_impulse_joint(
            cursor,
            p.body,
            RevoluteJointBuilder::new()
                .local_anchor1(Vector::ZERO)
                .local_anchor2(local)
                .contacts_enabled(false)
                // Мягкий шарнир: окно чуть «тянется» за курсором и не
                // вдавливает соседей бесконечной силой.
                .softness(SpringCoefficients::new(30.0, 1.0)),
        );
        self.drag = Some(Drag {
            piece,
            cursor,
            joint,
            target: world,
        });
        if let Some(body) = self.world.bodies.get_mut(p.body) {
            body.wake_up(true);
        }
        true
    }

    pub fn drag_update(&mut self, world: Vector) {
        if let Some(d) = self.drag.as_mut() {
            d.target = world;
        }
    }

    pub fn drag_piece(&self) -> Option<u32> {
        self.drag.as_ref().map(|d| d.piece)
    }

    /// Отпускание: окно летит дальше с набранной скоростью (бросок), но не
    /// быстрее `max_drag_speed`.
    pub fn drag_end(&mut self) {
        let Some(d) = self.drag.take() else {
            return;
        };
        self.world.remove_impulse_joint(d.joint);
        self.world.remove_body(d.cursor);
        let max = self.cfg.max_drag_speed;
        if let Some(body) = self
            .pieces
            .get(&d.piece)
            .and_then(|p| self.world.bodies.get_mut(p.body))
        {
            let v = body.linvel();
            let speed = v.length();
            if speed > max {
                body.set_linvel(v * (max / speed), true);
            }
        }
    }

    /// Двигает тело-курсор к цели не быстрее `max_drag_speed` и не ниже пола.
    /// Парящий (кинематический) кусок шарниром не сдвинуть — его ведём сами.
    fn update_drag_target(&mut self, dt: f32) {
        let Some(d) = self.drag.as_ref() else {
            return;
        };
        let Some(cursor) = self.world.bodies.get(d.cursor) else {
            return;
        };
        let cur = cursor.translation();
        let mut target = d.target;
        target.y = target.y.min(self.floor_y - 2.0);
        let delta = target - cur;
        let max_step = self.cfg.max_drag_speed * dt;
        let step = if delta.length() > max_step {
            delta * (max_step / delta.length())
        } else {
            delta
        };
        let (cursor_handle, joint, piece) = (d.cursor, d.joint, d.piece);
        if let Some(cursor) = self.world.bodies.get_mut(cursor_handle) {
            cursor.set_next_kinematic_translation(cur + step);
        }
        let anchor = self
            .world
            .impulse_joints
            .get(joint)
            .map(|j| j.data.local_anchor2());
        if let (Some(p), Some(anchor)) = (self.pieces.get(&piece), anchor) {
            if let Some(body) = self.world.bodies.get_mut(p.body) {
                if body.is_kinematic() {
                    let offset = body.rotation().transform_vector(anchor);
                    body.set_next_kinematic_translation(cur + step - offset);
                }
            }
        }
    }

    // ── Линии ──────────────────────────────────────────────────────

    /// Добавляет нарисованную линию. Для верёвки `pinned` = закреплённые концы
    /// (`None` — из конфига). Возвращает id линии.
    pub fn add_line(
        &mut self,
        material: Material,
        points: &[Vector],
        pinned: Option<[bool; 2]>,
    ) -> Option<u32> {
        let points = dedup_points(points, 1.0);
        if points.len() < 2 {
            return None;
        }
        let id = self.alloc_id();
        let kind = match material {
            Material::Rope => {
                let cfg = &self.materials.rope;
                let pinned = pinned.unwrap_or(match cfg.pin.trim() {
                    "both" => [true, true],
                    "none" => [false, false],
                    _ => [true, false],
                });
                let pts = resample(
                    &points,
                    cfg.segment_length.max(4.0),
                    cfg.segment_count.max(1) as usize,
                );
                let chain = self.build_chain(&pts, cfg.thickness, [0, 0], pinned)?;
                LineKind::Rope(chain)
            }
            _ => {
                let (friction, restitution, thickness, friction_rule, restitution_rule) =
                    match material {
                        Material::Steel => (
                            self.materials.steel.friction,
                            self.materials.steel.restitution,
                            self.materials.steel.thickness,
                            CoefficientCombineRule::Average,
                            CoefficientCombineRule::Average,
                        ),
                        Material::Glue => (
                            self.materials.glue.friction,
                            self.materials.glue.restitution,
                            self.materials.glue.thickness,
                            CoefficientCombineRule::Max,
                            CoefficientCombineRule::Min,
                        ),
                        _ => (
                            self.materials.trampoline.friction,
                            self.materials.trampoline.restitution,
                            self.materials.trampoline.thickness,
                            CoefficientCombineRule::Average,
                            CoefficientCombineRule::Max,
                        ),
                    };
                let radius = (thickness * 0.5).max(1.0);
                let body = self.world.insert_body(RigidBodyBuilder::fixed());
                let mut colliders = Vec::with_capacity(points.len());
                for w in points.windows(2) {
                    let c = self.world.insert_collider(
                        ColliderBuilder::capsule_from_endpoints(w[0], w[1], radius)
                            .friction(friction)
                            .restitution(restitution)
                            .friction_combine_rule(friction_rule)
                            .restitution_combine_rule(restitution_rule)
                            .user_data(Tag::Line(id).encode()),
                        Some(body),
                    );
                    colliders.push(c);
                }
                LineKind::Static {
                    body,
                    colliders,
                    points,
                }
            }
        };
        self.lines.insert(id, Line { material, kind });
        self.world.wake_up_all(true);
        Some(id)
    }

    pub fn remove_line(&mut self, id: u32) {
        let Some(line) = self.lines.remove(&id) else {
            return;
        };
        self.glue.retain(|g| {
            if g.line == id {
                self.world.impulse_joints.remove(g.joint, true);
                false
            } else {
                true
            }
        });
        match line.kind {
            LineKind::Static { body, .. } => {
                self.world.remove_body(body);
            }
            LineKind::Rope(chain) => self.destroy_chain(chain),
        }
        self.world.wake_up_all(true);
    }

    pub fn clear_lines(&mut self) {
        let ids: Vec<u32> = self.lines.keys().copied().collect();
        for id in ids {
            self.remove_line(id);
        }
    }

    /// Линия, ближайшая к мировой точке в пределах `radius`.
    pub fn line_at(&self, p: Vector, radius: f32) -> Option<u32> {
        let mut best: Option<(u32, f32)> = None;
        for view in self.line_views() {
            let half = Material::thickness(view.material, &self.materials) * 0.5;
            for w in view.points.windows(2) {
                let d = dist_to_segment(p, w[0], w[1]) - half;
                if d <= radius && best.map_or(true, |(_, bd)| d < bd) {
                    best = Some((view.id, d));
                }
            }
        }
        best.map(|(id, _)| id)
    }

    pub fn line_views(&self) -> Vec<LineView> {
        let mut out: Vec<LineView> = self
            .lines
            .iter()
            .map(|(&id, line)| match &line.kind {
                LineKind::Static { points, .. } => LineView {
                    id,
                    material: line.material,
                    points: points.clone(),
                    pinned: [true, true],
                },
                LineKind::Rope(chain) => LineView {
                    id,
                    material: line.material,
                    points: self.chain_points(chain),
                    pinned: chain.pinned,
                },
            })
            .collect();
        out.sort_by_key(|v| v.id);
        out
    }

    // ── Цепи (верёвки) ─────────────────────────────────────────────

    /// Строит цепь звеньев по точкам `pts` (каждая пара — одно звено).
    fn build_chain(
        &mut self,
        pts: &[Vector],
        thickness: f32,
        excluded: [u32; 2],
        pinned: [bool; 2],
    ) -> Option<RopeChain> {
        if pts.len() < 2 {
            return None;
        }
        let radius = (thickness * 0.5).max(1.0);
        let stiffness = self.materials.rope.joint_stiffness.clamp(0.0, 1.0);
        let softness = if stiffness >= 0.999 {
            SpringCoefficients::joint_defaults()
        } else {
            SpringCoefficients::new(5.0 + stiffness * 120.0, 1.0)
        };
        let mut segments = Vec::with_capacity(pts.len() - 1);
        let mut joints = Vec::new();
        let mut half_len = 0.0f32;
        for w in pts.windows(2) {
            let d = w[1] - w[0];
            let len = d.length().max(1.0);
            half_len = half_len.max(len * 0.5);
            let (body, _) = self.world.insert(
                RigidBodyBuilder::dynamic()
                    .translation((w[0] + w[1]) * 0.5)
                    .rotation(d.y.atan2(d.x))
                    .linear_damping(0.5)
                    .angular_damping(1.0)
                    .ccd_enabled(true),
                ColliderBuilder::capsule_x((len * 0.5 - radius).max(0.5), radius)
                    .density(self.cfg.density * 4.0)
                    .friction(0.6)
                    .active_hooks(ActiveHooks::FILTER_CONTACT_PAIRS)
                    .user_data(Tag::Rope { excluded }.encode()),
            );
            segments.push(body);
        }
        // Шарниры между соседними звеньями: конец i ↔ начало i+1.
        for i in 0..segments.len().saturating_sub(1) {
            let len_i = (pts[i + 1] - pts[i]).length() * 0.5;
            let len_j = (pts[i + 2] - pts[i + 1]).length() * 0.5;
            let j = self.world.insert_impulse_joint(
                segments[i],
                segments[i + 1],
                RevoluteJointBuilder::new()
                    .local_anchor1(Vector::new(len_i, 0.0))
                    .local_anchor2(Vector::new(-len_j, 0.0))
                    .contacts_enabled(false)
                    .softness(softness),
            );
            joints.push(j);
        }
        let mut chain = RopeChain {
            segments,
            half_len,
            joints,
            anchors: Vec::new(),
            pinned,
            hook: None,
        };
        for (end, pin) in pinned.into_iter().enumerate() {
            if pin {
                let p = if end == 0 { pts[0] } else { pts[pts.len() - 1] };
                let anchor = self
                    .world
                    .insert_body(RigidBodyBuilder::fixed().translation(p));
                let (seg, local) = self.chain_end(&chain, end);
                let j = self.world.insert_impulse_joint(
                    anchor,
                    seg,
                    RevoluteJointBuilder::new()
                        .local_anchor1(Vector::ZERO)
                        .local_anchor2(local),
                );
                chain.joints.push(j);
                chain.anchors.push(anchor);
            }
        }
        Some(chain)
    }

    /// Звено и локальная точка конца цепи (`end` = 0 начало, 1 конец).
    fn chain_end(&self, chain: &RopeChain, end: usize) -> (RigidBodyHandle, Vector) {
        let seg = if end == 0 {
            chain.segments[0]
        } else {
            chain.segments[chain.segments.len() - 1]
        };
        let half = self.segment_half_len(seg).unwrap_or(chain.half_len);
        (
            seg,
            Vector::new(if end == 0 { -half } else { half }, 0.0),
        )
    }

    fn segment_half_len(&self, seg: RigidBodyHandle) -> Option<f32> {
        let body = self.world.bodies.get(seg)?;
        let c = self.world.colliders.get(*body.colliders().first()?)?;
        let cap = c.shape().as_capsule()?;
        Some(cap.half_height() + cap.radius)
    }

    fn chain_points(&self, chain: &RopeChain) -> Vec<Vector> {
        let mut out = Vec::with_capacity(chain.segments.len() + 1);
        for (i, &seg) in chain.segments.iter().enumerate() {
            let Some(body) = self.world.bodies.get(seg) else {
                continue;
            };
            let half = self.segment_half_len(seg).unwrap_or(chain.half_len);
            let pos = body.position();
            if i == 0 {
                out.push(pos.transform_point(Vector::new(-half, 0.0)));
            }
            out.push(pos.transform_point(Vector::new(half, 0.0)));
        }
        out
    }

    fn destroy_chain(&mut self, chain: RopeChain) {
        for j in chain.joints {
            self.world.remove_impulse_joint(j);
        }
        if let Some((_, joints)) = chain.hook {
            for j in joints {
                self.world.remove_impulse_joint(j);
            }
        }
        for b in chain.anchors.into_iter().chain(chain.segments) {
            self.world.remove_body(b);
        }
    }

    /// Свободный конец нарисованной верёвки цепляется за окно, которое
    /// пользователь подносит к нему мышью (концепт §2: «на неё можно
    /// физически подвесить окно»).
    fn process_rope_hooks(&mut self) {
        if !self.materials.rope.hook_windows {
            return;
        }
        let Some(piece) = self.drag.as_ref().map(|d| d.piece) else {
            return;
        };
        let Some(p) = self.pieces.get(&piece) else {
            return;
        };
        let Some(pose) = self.world.bodies.get(p.body).map(|b| *b.position()) else {
            return;
        };
        let half = p.half;
        let piece_body = p.body;
        let reach = self.materials.rope.thickness + 6.0;
        let mut attach: Option<(u32, usize, Vector)> = None;
        for (&id, line) in &self.lines {
            let LineKind::Rope(chain) = &line.kind else {
                continue;
            };
            if chain.hook.is_some() {
                continue;
            }
            for end in 0..2 {
                if chain.pinned[end] {
                    continue;
                }
                let (seg, local) = self.chain_end(chain, end);
                let Some(seg_pose) = self.world.bodies.get(seg).map(|b| *b.position()) else {
                    continue;
                };
                let world = seg_pose.transform_point(local);
                let l = pose.inverse_transform_point(world);
                if l.x.abs() <= half.x + reach && l.y.abs() <= half.y + reach {
                    let clamped = Vector::new(l.x.clamp(-half.x, half.x), l.y.clamp(-half.y, half.y));
                    attach = Some((id, end, clamped));
                    break;
                }
            }
            if attach.is_some() {
                break;
            }
        }
        let Some((line_id, end, local_on_piece)) = attach else {
            return;
        };
        let (seg, seg_local, pin_anchor, length) = {
            let Some(Line {
                kind: LineKind::Rope(chain),
                ..
            }) = self.lines.get(&line_id)
            else {
                return;
            };
            let (seg, seg_local) = self.chain_end(chain, end);
            let length: f32 = self
                .chain_points(chain)
                .windows(2)
                .map(|w| (w[1] - w[0]).length())
                .sum();
            // Противоположный конец закреплён → страховочное ограничение длины.
            let pin_anchor = if chain.pinned[1 - end] {
                chain.anchors.first().copied()
            } else {
                None
            };
            (seg, seg_local, pin_anchor, length)
        };
        let mut joints = vec![self.world.insert_impulse_joint(
            piece_body,
            seg,
            RevoluteJointBuilder::new()
                .local_anchor1(local_on_piece)
                .local_anchor2(seg_local)
                .contacts_enabled(false),
        )];
        if let Some(anchor) = pin_anchor {
            joints.push(self.world.insert_impulse_joint(
                anchor,
                piece_body,
                RopeJointBuilder::new(length.max(1.0))
                    .local_anchor1(Vector::ZERO)
                    .local_anchor2(local_on_piece),
            ));
        }
        // Звенья больше не толкаются с подвешенным окном.
        if let Some(Line {
            kind: LineKind::Rope(chain),
            ..
        }) = self.lines.get_mut(&line_id)
        {
            chain.hook = Some((piece, joints));
            for &s in &chain.segments {
                if let Some(body) = self.world.bodies.get(s) {
                    for &c in body.colliders() {
                        if let Some(col) = self.world.colliders.get_mut(c) {
                            col.user_data = Tag::Rope {
                                excluded: [piece, 0],
                            }
                            .encode();
                        }
                    }
                }
            }
        }
    }

    // ── Клей ───────────────────────────────────────────────────────

    /// Приклеивает куски, коснувшиеся клеевых линий, и обновляет кулдауны.
    fn process_glue(&mut self, dt: f32) {
        self.glue_cooldown.retain(|_, t| {
            *t -= dt;
            *t > 0.0
        });
        let glued: HashSet<(u32, u32)> = self.glue.iter().map(|g| (g.piece, g.line)).collect();
        let mut new_bonds: Vec<(u32, u32, RigidBodyHandle)> = Vec::new();
        for (&line_id, line) in &self.lines {
            if line.material != Material::Glue {
                continue;
            }
            let LineKind::Static {
                body, colliders, ..
            } = &line.kind
            else {
                continue;
            };
            for &c in colliders {
                for pair in self.world.narrow_phase.contact_pairs_with(c) {
                    if !pair.has_any_active_contact() {
                        continue;
                    }
                    let other = if pair.collider1 == c {
                        pair.collider2
                    } else {
                        pair.collider1
                    };
                    let Some(col) = self.world.colliders.get(other) else {
                        continue;
                    };
                    let Tag::Piece(piece) = Tag::decode(col.user_data) else {
                        continue;
                    };
                    if glued.contains(&(piece, line_id))
                        || self.glue_cooldown.contains_key(&piece)
                        || new_bonds.iter().any(|b| b.0 == piece && b.1 == line_id)
                    {
                        continue;
                    }
                    new_bonds.push((piece, line_id, *body));
                }
            }
        }
        for (piece, line, line_body) in new_bonds {
            let Some(p) = self.pieces.get(&piece) else {
                continue;
            };
            let Some(piece_pose) = self.world.bodies.get(p.body).map(|b| *b.position()) else {
                continue;
            };
            let Some(line_pose) = self.world.bodies.get(line_body).map(|b| *b.position()) else {
                continue;
            };
            // Шов фиксирует кусок в текущей позе — без рывка.
            let frame1 = line_pose.inverse() * piece_pose;
            let joint = self.world.insert_impulse_joint(
                line_body,
                p.body,
                FixedJointBuilder::new()
                    .local_frame1(frame1)
                    .local_frame2(Pose::IDENTITY),
            );
            self.glue.push(GlueBond { piece, line, joint });
        }
    }

    /// Рвёт клей и разрушимые связи, если шов «гасит» слишком большую Δv.
    fn process_breaks(&mut self) {
        let detach = self.materials.glue.detach_force;
        let mut broken_glue = Vec::new();
        for (i, g) in self.glue.iter().enumerate() {
            if detach <= 0.0 {
                break;
            }
            if let Some(j) = self.world.impulse_joints.get(g.joint) {
                let imp = j.impulses;
                let dv = Vector::new(imp.x, imp.y).length() / self.piece_mass(g.piece);
                if dv > detach {
                    broken_glue.push(i);
                }
            }
        }
        for i in broken_glue.into_iter().rev() {
            let g = self.glue.remove(i);
            self.world.remove_impulse_joint(g.joint);
            self.glue_cooldown.insert(g.piece, 0.6);
        }

        let mut broken_links = Vec::new();
        for (&id, link) in &self.links {
            if link.break_dv <= 0.0 {
                continue;
            }
            for &jh in &link.joints {
                if let Some(j) = self.world.impulse_joints.get(jh) {
                    let imp = j.impulses;
                    let mass = self.piece_mass(link.a.piece).min(self.piece_mass(link.b.piece));
                    if Vector::new(imp.x, imp.y).length() / mass > link.break_dv {
                        broken_links.push(id);
                        break;
                    }
                }
            }
        }
        for id in broken_links {
            self.remove_link(id);
        }
    }

    // ── Связи между окнами ─────────────────────────────────────────

    /// Связывает два куска. `a`/`b` — точки крепления в локальных координатах.
    pub fn add_link(&mut self, kind: LinkKind, a: Anchor, b: Anchor) -> Option<u32> {
        if a.piece == b.piece {
            return None;
        }
        let body_a = self.pieces.get(&a.piece)?.body;
        let body_b = self.pieces.get(&b.piece)?.body;
        let pose_a = *self.world.bodies.get(body_a)?.position();
        let pose_b = *self.world.bodies.get(body_b)?.position();
        let wa = pose_a.transform_point(a.local);
        let wb = pose_b.transform_point(b.local);
        let mut joints = Vec::new();
        let mut rope = None;
        let mut break_dv = 0.0;
        match kind {
            LinkKind::Weld => {
                // Монолит в текущей взаимной позе: frame2 = поза b в системе a.
                let rel = pose_a.inverse() * pose_b;
                joints.push(self.world.insert_impulse_joint(
                    body_a,
                    body_b,
                    FixedJointBuilder::new()
                        .local_frame1(rel)
                        .local_frame2(Pose::IDENTITY)
                        .contacts_enabled(false),
                ));
                break_dv = self.joints_cfg.weld.break_force.max(0.0);
            }
            LinkKind::Spring => {
                let cfg = &self.joints_cfg.spring;
                let rest = if cfg.rest_length > 0.0 {
                    cfg.rest_length
                } else {
                    (wb - wa).length().max(1.0)
                };
                joints.push(
                    self.world.insert_impulse_joint(
                        body_a,
                        body_b,
                        SpringJointBuilder::new(rest, cfg.stiffness.max(0.0), cfg.damping.max(0.0))
                            .spring_model(MotorModel::AccelerationBased)
                            .local_anchor1(a.local)
                            .local_anchor2(b.local),
                    ),
                );
            }
            LinkKind::Rope => {
                let cfg = self.joints_cfg.rope.clone();
                let dist = (wb - wa).length();
                let length = (dist * 1.1).min(cfg.max_length.max(dist)).max(1.0);
                let pts = sagging_curve(wa, wb, length, cfg.segment_length.max(4.0));
                let excluded = if cfg.collides_with_linked_windows {
                    [0, 0]
                } else {
                    [a.piece, b.piece]
                };
                let thickness = self.materials.rope.thickness;
                let mut chain = self.build_chain(&pts, thickness, excluded, [false, false])?;
                let (seg0, l0) = self.chain_end(&chain, 0);
                let (seg1, l1) = self.chain_end(&chain, 1);
                chain.joints.push(self.world.insert_impulse_joint(
                    body_a,
                    seg0,
                    RevoluteJointBuilder::new()
                        .local_anchor1(a.local)
                        .local_anchor2(l0)
                        .contacts_enabled(false),
                ));
                chain.joints.push(self.world.insert_impulse_joint(
                    body_b,
                    seg1,
                    RevoluteJointBuilder::new()
                        .local_anchor1(b.local)
                        .local_anchor2(l1)
                        .contacts_enabled(false),
                ));
                // Лёгкая цепь под тяжёлым окном (перепад масс ~40:1) заметно
                // тянется в PGS-солвере. Страховочный rope joint между точками
                // крепления не даёт окнам разойтись дальше длины верёвки.
                chain.joints.push(self.world.insert_impulse_joint(
                    body_a,
                    body_b,
                    RopeJointBuilder::new(length)
                        .local_anchor1(a.local)
                        .local_anchor2(b.local),
                ));
                rope = Some(chain);
            }
        }
        let id = self.alloc_id();
        self.links.insert(
            id,
            Link {
                kind,
                a,
                b,
                joints,
                rope,
                break_dv,
            },
        );
        self.world.wake_up(body_a, true);
        self.world.wake_up(body_b, true);
        Some(id)
    }

    pub fn remove_link(&mut self, id: u32) {
        let Some(link) = self.links.remove(&id) else {
            return;
        };
        for j in link.joints {
            self.world.remove_impulse_joint(j);
        }
        if let Some(chain) = link.rope {
            self.destroy_chain(chain);
        }
    }

    /// Снимает с куска все связи, клей и подвесы на верёвках.
    pub fn unlink_piece(&mut self, piece: u32) -> bool {
        let mut changed = false;
        let ids: Vec<u32> = self
            .links
            .iter()
            .filter(|(_, l)| l.a.piece == piece || l.b.piece == piece)
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.remove_link(id);
            changed = true;
        }
        let world = &mut self.world;
        self.glue.retain(|g| {
            if g.piece == piece {
                world.impulse_joints.remove(g.joint, true);
                changed = true;
                false
            } else {
                true
            }
        });
        if changed {
            self.glue_cooldown.insert(piece, 0.6);
        }
        for line in self.lines.values_mut() {
            if let LineKind::Rope(chain) = &mut line.kind {
                if chain.hook.as_ref().map_or(false, |(p, _)| *p == piece) {
                    if let Some((_, joints)) = chain.hook.take() {
                        for j in joints {
                            self.world.impulse_joints.remove(j, true);
                        }
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn link_views(&self) -> Vec<LinkView> {
        let mut out: Vec<LinkView> = self
            .links
            .iter()
            .filter_map(|(&id, link)| {
                let points = match &link.rope {
                    Some(chain) => self.chain_points(chain),
                    None => {
                        let (ca, aa) = self.piece_transform(link.a.piece)?;
                        let (cb, ab) = self.piece_transform(link.b.piece)?;
                        vec![
                            Pose::new(ca, aa).transform_point(link.a.local),
                            Pose::new(cb, ab).transform_point(link.b.local),
                        ]
                    }
                };
                Some(LinkView {
                    id,
                    kind: link.kind,
                    a: link.a,
                    b: link.b,
                    points,
                })
            })
            .collect();
        out.sort_by_key(|v| v.id);
        out
    }

    pub fn has_links(&self, piece: u32) -> bool {
        self.links
            .values()
            .any(|l| l.a.piece == piece || l.b.piece == piece)
            || self.glue.iter().any(|g| g.piece == piece)
    }
}

// ── Геометрия ──────────────────────────────────────────────────────

/// Убирает почти совпадающие соседние точки.
pub fn dedup_points(points: &[Vector], min_dist: f32) -> Vec<Vector> {
    let mut out: Vec<Vector> = Vec::with_capacity(points.len());
    for &p in points {
        if !p.is_finite() {
            continue;
        }
        if out.last().map_or(true, |l| (p - *l).length() >= min_dist) {
            out.push(p);
        }
    }
    out
}

/// Перевыборка ломаной с шагом `step`, не больше `max_segments` звеньев.
pub fn resample(points: &[Vector], step: f32, max_segments: usize) -> Vec<Vector> {
    let total: f32 = points.windows(2).map(|w| (w[1] - w[0]).length()).sum();
    if total <= f32::EPSILON || points.len() < 2 {
        return points.to_vec();
    }
    let n = ((total / step).ceil() as usize).clamp(1, max_segments.max(1));
    let seg = total / n as f32;
    let mut out = Vec::with_capacity(n + 1);
    out.push(points[0]);
    let mut i = 0;
    let mut t_in_edge = 0.0f32;
    for k in 1..n {
        let mut remaining = seg;
        loop {
            let a = points[i];
            let b = points[i + 1];
            let edge = (b - a).length();
            let left = edge - t_in_edge;
            if remaining <= left || i + 2 >= points.len() {
                t_in_edge += remaining;
                let t = if edge > 0.0 { (t_in_edge / edge).min(1.0) } else { 0.0 };
                out.push(a + (b - a) * t);
                break;
            }
            remaining -= left;
            i += 1;
            t_in_edge = 0.0;
        }
        let _ = k;
    }
    out.push(points[points.len() - 1]);
    out
}

/// Провисающая кривая длиной ≈ `length` от `a` до `b` (для верёвки-связи).
fn sagging_curve(a: Vector, b: Vector, length: f32, step: f32) -> Vec<Vector> {
    let n = ((length / step).ceil() as usize).clamp(2, 128);
    let curve = |sag: f32| -> Vec<Vector> {
        (0..=n)
            .map(|i| {
                let t = i as f32 / n as f32;
                a + (b - a) * t + Vector::new(0.0, sag * 4.0 * t * (1.0 - t))
            })
            .collect()
    };
    let len_of = |pts: &[Vector]| -> f32 { pts.windows(2).map(|w| (w[1] - w[0]).length()).sum() };
    let dist = (b - a).length();
    if length <= dist + 0.5 {
        return curve(0.0);
    }
    // Бисекция по стреле провиса.
    let (mut lo, mut hi) = (0.0f32, length);
    for _ in 0..30 {
        let mid = (lo + hi) * 0.5;
        if len_of(&curve(mid)) < length {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    curve(lo)
}

pub fn dist_to_segment(p: Vector, a: Vector, b: Vector) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 > 0.0 {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p - (a + ab * t)).length()
}

/// Поворот вектора на угол.
pub fn rotate(v: Vector, angle: f32) -> Vector {
    Rotation::new(angle).transform_vector(v)
}

#[allow(dead_code)]
fn _assert_real(_: Real) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> WindowPhysics {
        WindowPhysics::new(&Config::default(), 1000.0)
    }

    fn run(p: &mut WindowPhysics, secs: f32) {
        let steps = (secs / p.dt()) as usize;
        for _ in 0..steps {
            p.step_once();
        }
    }

    #[test]
    fn tags_roundtrip() {
        for t in [
            Tag::None,
            Tag::Floor,
            Tag::Piece(7),
            Tag::Line(u32::MAX),
            Tag::Rope {
                excluded: [3, 900],
            },
        ] {
            assert_eq!(Tag::decode(t.encode()), t);
        }
    }

    #[test]
    fn piece_lands_on_floor() {
        let mut p = world();
        let id = p.spawn_piece(Vector::new(0.0, 0.0), 0.0, 400.0, 300.0, 1.0, false);
        run(&mut p, 4.0);
        let (c, _) = p.piece_transform(id).unwrap();
        assert!((c.y - (1000.0 - 150.0)).abs() < 6.0, "center {c:?}");
    }

    #[test]
    fn steel_shelf_holds_window() {
        let mut p = world();
        p.add_line(
            Material::Steel,
            &[Vector::new(-400.0, 500.0), Vector::new(400.0, 500.0)],
            None,
        )
        .unwrap();
        let id = p.spawn_piece(Vector::new(0.0, 200.0), 0.0, 200.0, 100.0, 1.0, false);
        run(&mut p, 3.0);
        let (c, _) = p.piece_transform(id).unwrap();
        assert!(c.y < 500.0, "fell through shelf: {c:?}");
    }

    #[test]
    fn glue_sticks_and_detaches_on_hard_pull() {
        let mut p = world();
        p.add_line(
            Material::Glue,
            &[Vector::new(-400.0, 500.0), Vector::new(400.0, 500.0)],
            None,
        )
        .unwrap();
        let id = p.spawn_piece(Vector::new(0.0, 380.0), 0.0, 200.0, 100.0, 1.0, false);
        run(&mut p, 2.0);
        assert_eq!(p.glue.len(), 1, "should be glued");
        // Слабо потянуть — держит.
        let (c, _) = p.piece_transform(id).unwrap();
        p.drag_begin(id, c);
        p.drag_update(c + Vector::new(0.0, -5.0));
        run(&mut p, 0.5);
        assert_eq!(p.glue.len(), 1, "gentle pull must not detach");
        // Резко дёрнуть — отрывается.
        p.drag_update(c + Vector::new(0.0, -600.0));
        run(&mut p, 1.0);
        p.drag_end();
        assert!(p.glue.is_empty(), "hard pull must detach");
    }

    #[test]
    fn floating_window_stays() {
        let mut p = world();
        let id = p.spawn_piece(Vector::new(0.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        p.set_floating(id, true);
        run(&mut p, 2.0);
        let (c, _) = p.piece_transform(id).unwrap();
        assert!(c.y.abs() < 1.0);
    }

    #[test]
    fn zero_gravity_floats() {
        let mut p = world();
        p.set_zero_gravity(true);
        let id = p.spawn_piece(Vector::new(0.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        run(&mut p, 2.0);
        assert!(p.piece_transform(id).unwrap().0.y.abs() < 1.0);
        p.set_zero_gravity(false);
        run(&mut p, 3.0);
        assert!(p.piece_transform(id).unwrap().0.y > 800.0);
    }

    #[test]
    fn rope_hangs_from_pin() {
        let mut p = world();
        let id = p
            .add_line(
                Material::Rope,
                &[Vector::new(0.0, 0.0), Vector::new(300.0, 0.0)],
                None,
            )
            .unwrap();
        run(&mut p, 4.0);
        let v = p.line_views().into_iter().find(|v| v.id == id).unwrap();
        let first = v.points[0];
        let last = *v.points.last().unwrap();
        assert!(first.length() < 3.0, "pinned end moved: {first:?}");
        assert!(last.y > 200.0, "rope should hang down: {last:?}");
    }

    #[test]
    fn weld_spring_rope_links() {
        let mut p = world();
        let a = p.spawn_piece(Vector::new(-200.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        let b = p.spawn_piece(Vector::new(200.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        p.set_floating(a, true);
        let weld = p
            .add_link(
                LinkKind::Weld,
                Anchor {
                    piece: a,
                    local: Vector::ZERO,
                },
                Anchor {
                    piece: b,
                    local: Vector::ZERO,
                },
            )
            .unwrap();
        run(&mut p, 2.0);
        let (ca, _) = p.piece_transform(a).unwrap();
        let (cb, _) = p.piece_transform(b).unwrap();
        assert!(((cb - ca).length() - 400.0).abs() < 10.0, "weld keeps distance");
        p.remove_link(weld);
        let rope = p
            .add_link(
                LinkKind::Rope,
                Anchor {
                    piece: a,
                    local: Vector::new(100.0, 0.0),
                },
                Anchor {
                    piece: b,
                    local: Vector::new(-100.0, 0.0),
                },
            )
            .unwrap();
        run(&mut p, 3.0);
        let (cb, _) = p.piece_transform(b).unwrap();
        assert!(cb.y < 700.0, "rope must hold b: {cb:?}");
        assert!(p.link_views().iter().any(|l| l.id == rope && l.points.len() > 3));
        assert!(p.unlink_piece(a));
        assert!(p.link_views().is_empty());
    }

    #[test]
    fn remove_piece_cleans_up() {
        let mut p = world();
        let a = p.spawn_piece(Vector::new(-200.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        let b = p.spawn_piece(Vector::new(200.0, 0.0), 0.0, 200.0, 100.0, 1.0, false);
        p.add_link(
            LinkKind::Spring,
            Anchor {
                piece: a,
                local: Vector::ZERO,
            },
            Anchor {
                piece: b,
                local: Vector::ZERO,
            },
        );
        p.drag_begin(a, Vector::new(-200.0, 0.0));
        p.remove_piece(a);
        assert!(p.drag_piece().is_none());
        assert!(p.link_views().is_empty());
        run(&mut p, 0.5);
    }

    #[test]
    fn resample_lengths() {
        let pts = [Vector::new(0.0, 0.0), Vector::new(100.0, 0.0), Vector::new(100.0, 100.0)];
        let r = resample(&pts, 20.0, 64);
        assert_eq!(r.len(), 11);
        for w in r.windows(2) {
            assert!(((w[1] - w[0]).length() - 20.0).abs() < 0.5);
        }
        let s = sagging_curve(Vector::ZERO, Vector::new(100.0, 0.0), 150.0, 10.0);
        let len: f32 = s.windows(2).map(|w| (w[1] - w[0]).length()).sum();
        assert!((len - 150.0).abs() < 2.0);
    }
}
