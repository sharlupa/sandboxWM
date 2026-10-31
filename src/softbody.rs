//! Мягкое тело для одного прямоугольного куска окна (концепт §5 и §6).
//!
//! Кусок окна — сетка из (res+1)×(res+1) частиц. Координаты ЛОКАЛЬНЫЕ: начало
//! в центре куска, пиксели, y растёт вниз. У каждой частицы фиксированный UV
//! (0..1, начало слева сверху) — точка плоского буфера клиента, которую частица
//! «несёт» на себе; поэтому текстура натягивается на сетку без пересчёта UV.
//!
//! Модель — position-based dynamics поверх Verlet:
//! - **целевая форма** `goal` = форма покоя `rest` (её меняет пластика) плюс
//!   упругая часть деформации от перетаскивания;
//! - каждая частица притягивается к своей точке целевой формы (якорная
//!   пружина, жёсткость из `softness`); при `softness == 0` сетка каждый шаг
//!   в точности равна целевой форме — окно жёсткое;
//! - ограничения длины по сторонам ячеек (структурные) и диагоналям (сдвиг)
//!   плюс сохранение площади ячеек: связывают соседей, дают «желейные» волны;
//! - сетка не выходит за выпуклую оболочку целевой формы — это и коллайдер
//!   окна, так что колышущееся окно не налезает на соседей и пол;
//! - инерционная псевдосила `−a` (ускорение жёсткой системы отсчёта, то есть
//!   rapier-тела окна). Она приложена с весом от «опорной» грани: грань, в
//!   которую давит сила, удерживается (её держит пол или препятствие), дальняя
//!   отстаёт. Опорную грань держит односторонняя плоскость, которая «помнит»
//!   контакт, пока сетка в неё упирается. Поэтому жёсткая посадка сплющивает
//!   окно, а потом оно колышется обратно к форме покоя;
//! - затухание скорости.
//!
//! Обратная проекция (§6): треугольники сетки рисуются GPU линейно, поэтому и
//! прямое отображение uv → local, и обратное local → uv считаются по тем же
//! треугольникам (барицентрически). Так они точно обратны друг другу, и клик
//! попадает ровно в тот пиксель буфера, который пользователь видит под курсором.

// Интеграция в state/render/input идёт отдельно — часть API пока не вызывается.
#![allow(dead_code)]

/// Параметры материала окна (настраиваются в конфиге).
#[derive(Clone, Copy, Debug)]
pub struct SoftParams {
    /// 0.0 — жёсткое окно (сетка в точности равна форме покоя), 1.0 — желе.
    pub softness: f32,
    /// 0..1: насколько резкое ускорение (удар) сплющивает сетку.
    pub impact_squash: f32,
    /// 0..1: какая доля деформации от перетаскивания остаётся навсегда.
    pub plasticity: f32,
}

impl Default for SoftParams {
    fn default() -> Self {
        Self {
            softness: 0.5,
            impact_squash: 0.5,
            plasticity: 0.0,
        }
    }
}

/// Непрозрачный хэндл захвата точки сетки (см. [`SoftMesh::grab`]).
#[derive(Debug, PartialEq, Eq)]
pub struct SoftGrab {
    id: u32,
}

/// Состояние одного захвата.
struct GrabState {
    id: u32,
    /// Точка материала (uv), за которую взяли.
    anchor_uv: [f32; 2],
    /// Смещение курсора от точки материала в момент захвата (если взяли чуть
    /// снаружи сетки — за край в пределах `radius`).
    offset: [f32; 2],
    /// Положение точки захвата в `base`.
    base_anchor: [f32; 2],
    /// Смещение точки захвата относительно `base` (с поправкой, см. `drag`).
    disp: [f32; 2],
    /// Пластичность и податливость на момент последнего `drag`
    /// (нужны в `release`, которому параметры не передаются).
    plastic: f32,
    compliance: f32,
    /// Вес влияния захвата на каждую частицу (ядро со спадом по `base`).
    weights: Vec<f32>,
}

/// Ограничение длины между двумя частицами.
#[derive(Clone, Copy)]
struct Edge {
    a: u16,
    b: u16,
    len: f32,
}

/// Ячейка сетки для ограничения площади: углы tl, tr, br, bl.
#[derive(Clone, Copy)]
struct Cell {
    q: [u16; 4],
    area: f32,
}

/// Мягкая сетка одного куска окна.
pub struct SoftMesh {
    width: f32,
    height: f32,
    res: usize,
    /// Фиксированные UV частиц.
    uv: Vec<[f32; 2]>,
    /// Текущие и предыдущие (Verlet) положения.
    pos: Vec<[f32; 2]>,
    prev: Vec<[f32; 2]>,
    /// Форма покоя (с учётом пластики).
    rest: Vec<[f32; 2]>,
    /// Форма покоя на момент начала захвата; во время захвата
    /// `rest = base + пластическая часть`.
    base: Vec<[f32; 2]>,
    /// Целевая форма, к которой тянется сетка.
    goal: Vec<[f32; 2]>,
    /// 0..1: насколько частица удерживается захватом (1 — прибита к цели).
    hold: Vec<f32>,
    /// Обратная масса для ограничений (удерживаемые частицы тяжелее).
    inv_mass: Vec<f32>,
    /// Scratch: вес инерционной псевдосилы для частицы.
    support: Vec<f32>,
    /// Опорная плоскость: направление последнего сильного удара (куда давит
    /// псевдосила), его «незабытая» величина и сколько ещё держать контакт, с.
    support_dir: [f32; 2],
    support_strength: f32,
    support_timer: f32,
    indices: Vec<u16>,
    /// Граничные частицы по кругу (по часовой стрелке на экране).
    boundary: Vec<u16>,
    structural: Vec<Edge>,
    shear: Vec<Edge>,
    cells: Vec<Cell>,
    grabs: Vec<GrabState>,
    next_grab_id: u32,
    active: bool,
    settled: bool,
    rest_generation: u64,
}

/// Нагрузка на окно без её постоянной части: сетку деформируют удары,
/// рывки и отрыв от пола, а постоянная нагрузка — вес на полу, прижатие
/// соседом — забывается за [`LOAD_RELAX`] с. Иначе мягкое окно навсегда
/// оставалось сплющенным под собственным весом.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadFilter {
    base: Option<[f32; 2]>,
}

/// За сколько секунд (постоянная времени) забывается постоянная нагрузка.
const LOAD_RELAX: f32 = 0.25;

impl LoadFilter {
    /// Изменение нагрузки `load` (собственное ускорение тела) за шаг `dt`.
    pub fn apply(&mut self, load: [f32; 2], dt: f32) -> [f32; 2] {
        let load = sanitize2(load);
        let base = self.base.get_or_insert(load);
        let out = sub(load, *base);
        let k = 1.0 - (-sanitize(dt).max(0.0) / LOAD_RELAX).exp();
        *base = add(*base, scale(out, k));
        out
    }
}

// --- Настройки модели (на шаг 1/60 с) ---

/// Минимальная жёсткость якорной пружины (softness = 1): ~1.5 Гц колебаний.
const ANCHOR_MIN: f32 = 0.025;
/// Затухание скорости за шаг 1/60 с при softness 0 и 1.
const DAMP_RIGID: f32 = 0.5;
const DAMP_JELLY: f32 = 0.045;
/// Итерации Гаусса—Зейделя для ограничений.
const ITERATIONS: usize = 4;
/// Шаг, под который подобраны константы.
const BASE_DT: f32 = 1.0 / 60.0;
/// Максимум подшагов за один вызов `step` (dt обрезается до 0.1 с).
const MAX_SUBSTEPS: usize = 6;
/// Порог «движется заметно»: px/с.
const ACTIVE_SPEED: f32 = 1.0;
/// Порог «деформирована»: px.
const DEFORMED_EPS: f32 = 0.5;
/// Сколько держать опорную плоскость после последнего касания, с.
const SUPPORT_MEMORY: f32 = 0.3;
/// Период полураспада «памяти» о силе удара, с.
const SUPPORT_HALF_LIFE: f32 = 0.25;

#[inline]
fn sub(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn add(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [a[0] + b[0], a[1] + b[1]]
}

#[inline]
fn scale(a: [f32; 2], s: f32) -> [f32; 2] {
    [a[0] * s, a[1] * s]
}

#[inline]
fn dot(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

#[inline]
fn cross(a: [f32; 2], b: [f32; 2]) -> f32 {
    a[0] * b[1] - a[1] * b[0]
}

#[inline]
fn len(a: [f32; 2]) -> f32 {
    dot(a, a).sqrt()
}

#[inline]
fn finite2(a: [f32; 2]) -> bool {
    a[0].is_finite() && a[1].is_finite()
}

/// NaN/inf → 0, чтобы мусор на входе не отравил симуляцию.
#[inline]
fn sanitize(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

#[inline]
fn sanitize2(a: [f32; 2]) -> [f32; 2] {
    [sanitize(a[0]), sanitize(a[1])]
}

/// Ограничивает длину вектора.
#[inline]
fn clamp_len(a: [f32; 2], max: f32) -> [f32; 2] {
    let l = len(a);
    if l > max && l > 0.0 {
        scale(a, max / l)
    } else {
        a
    }
}

/// Выпуклая оболочка точек (монотонная цепь), обход против часовой стрелки
/// в координатах с осью y вниз — по часовой на экране.
fn convex_hull(points: &[[f32; 2]]) -> Vec<[f32; 2]> {
    let mut pts: Vec<[f32; 2]> = points.iter().copied().filter(|p| finite2(*p)).collect();
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let turn = |o: [f32; 2], a: [f32; 2], b: [f32; 2]| cross(sub(a, o), sub(b, o));
    let mut hull: Vec<[f32; 2]> = Vec::with_capacity(pts.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &[f32; 2]>> = if pass == 0 {
            Box::new(pts.iter())
        } else {
            Box::new(pts.iter().rev())
        };
        for &p in iter {
            while hull.len() >= start + 2
                && turn(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// Точка `p`, возвращённая внутрь выпуклого многоугольника `hull` (обход
/// как у [`convex_hull`]); `None` — она и так внутри.
fn project_inside(hull: &[[f32; 2]], p: [f32; 2]) -> Option<[f32; 2]> {
    let n = hull.len();
    if n < 3 {
        return None;
    }
    let outside = (0..n).any(|i| cross(sub(hull[(i + 1) % n], hull[i]), sub(p, hull[i])) < 0.0);
    if !outside {
        return None;
    }
    (0..n)
        .map(|i| {
            let (a, b) = (hull[i], hull[(i + 1) % n]);
            let (t, d2) = closest_on_segment(p, a, b);
            (add(a, scale(sub(b, a), t)), d2)
        })
        .min_by(|x, y| x.1.total_cmp(&y.1))
        .map(|(q, _)| q)
}

/// Ближайшая к `p` точка отрезка `a..b`: (параметр t, расстояние²).
fn closest_on_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> (f32, f32) {
    let ab = sub(b, a);
    let l2 = dot(ab, ab);
    let t = if l2 > 1e-12 {
        (dot(sub(p, a), ab) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let c = add(a, scale(ab, t));
    let d = sub(p, c);
    (t, dot(d, d))
}

/// Барицентрические координаты `p` в треугольнике (a, b, c).
/// `None` для вырожденного треугольника.
#[inline]
fn barycentric(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> Option<[f32; 3]> {
    let v0 = sub(b, a);
    let v1 = sub(c, a);
    let den = cross(v0, v1);
    if den.abs() < 1e-6 {
        return None;
    }
    let v2 = sub(p, a);
    let wb = cross(v2, v1) / den;
    let wc = cross(v0, v2) / den;
    Some([1.0 - wb - wc, wb, wc])
}

/// Ядро спада влияния захвата: гладкое, с компактным носителем радиуса `r`.
#[inline]
fn falloff(dist: f32, r: f32) -> f32 {
    let x = dist / r;
    if x >= 1.0 {
        0.0
    } else {
        let k = 1.0 - x * x;
        k * k * k
    }
}

/// Насколько упругая часть перетаскивания доходит до сетки: жёсткое окно
/// (softness 0) не тянется вовсе, начиная с softness 0.25 — полностью.
#[inline]
fn compliance(softness: f32) -> f32 {
    (softness * 4.0).clamp(0.0, 1.0)
}

impl SoftMesh {
    /// Новая плоская сетка `width`×`height` px; `resolution` (ячеек на сторону)
    /// обрезается до 1..=32.
    pub fn new(width: f32, height: f32, resolution: u32) -> Self {
        let res = resolution.clamp(1, 32) as usize;
        let n = (res + 1) * (res + 1);
        let width = Self::sane_extent(width);
        let height = Self::sane_extent(height);

        let mut uv = Vec::with_capacity(n);
        for y in 0..=res {
            for x in 0..=res {
                uv.push([x as f32 / res as f32, y as f32 / res as f32]);
            }
        }

        let mut mesh = Self {
            width,
            height,
            res,
            uv,
            pos: vec![[0.0; 2]; n],
            prev: vec![[0.0; 2]; n],
            rest: vec![[0.0; 2]; n],
            base: vec![[0.0; 2]; n],
            goal: vec![[0.0; 2]; n],
            hold: vec![0.0; n],
            inv_mass: vec![1.0; n],
            support: vec![0.0; n],
            support_dir: [0.0, 1.0],
            support_strength: 0.0,
            support_timer: 0.0,
            indices: Vec::with_capacity(res * res * 6),
            boundary: Vec::with_capacity(res * 4),
            structural: Vec::with_capacity(2 * res * (res + 1)),
            shear: Vec::with_capacity(res * res),
            cells: Vec::with_capacity(res * res),
            grabs: Vec::new(),
            next_grab_id: 1,
            active: false,
            settled: true,
            rest_generation: 0,
        };
        mesh.build_topology();
        mesh.reset();
        mesh
    }

    fn sane_extent(v: f32) -> f32 {
        if v.is_finite() { v.max(1.0) } else { 1.0 }
    }

    #[inline]
    fn idx(&self, x: usize, y: usize) -> usize {
        y * (self.res + 1) + x
    }

    /// Индексы треугольников, граница и списки ограничений. Диагональ ячеек
    /// чередуется в шахматном порядке, чтобы деформации выглядели симметрично.
    ///
    /// Ограничения разложены по «цветам»: внутри цвета соседние в списке
    /// ограничения не делят частиц, и процессор считает их параллельно
    /// (иначе каждый шаг Гаусса—Зейделя ждёт sqrt/деление предыдущего — это
    /// втрое медленнее).
    fn build_topology(&mut self) {
        let r = self.res;
        self.indices.clear();
        self.cells.clear();
        self.structural.clear();
        self.shear.clear();
        self.boundary.clear();

        for cy in 0..r {
            for cx in 0..r {
                let tl = self.idx(cx, cy) as u16;
                let tr = self.idx(cx + 1, cy) as u16;
                let bl = self.idx(cx, cy + 1) as u16;
                let br = self.idx(cx + 1, cy + 1) as u16;
                if (cx + cy).is_multiple_of(2) {
                    // Диагональ tl–br.
                    self.indices.extend_from_slice(&[tl, tr, br, tl, br, bl]);
                } else {
                    // Диагональ tr–bl.
                    self.indices.extend_from_slice(&[tl, tr, bl, tr, br, bl]);
                }
            }
        }
        // Ячейки (площадь) и сдвиг — 4 цвета по чётности (cx, cy): ячейки
        // одного цвета не соприкасаются даже углами. Сдвиговое ограничение —
        // по той же диагонали, что делит ячейку на треугольники: каждый
        // треугольник закреплён по всем сторонам.
        for color in 0..4 {
            for cy in (color / 2..r).step_by(2) {
                for cx in (color % 2..r).step_by(2) {
                    let tl = self.idx(cx, cy) as u16;
                    let tr = self.idx(cx + 1, cy) as u16;
                    let bl = self.idx(cx, cy + 1) as u16;
                    let br = self.idx(cx + 1, cy + 1) as u16;
                    self.cells.push(Cell {
                        q: [tl, tr, br, bl],
                        area: 0.0,
                    });
                    let (a, b) = if (cx + cy).is_multiple_of(2) {
                        (tl, br)
                    } else {
                        (tr, bl)
                    };
                    self.shear.push(Edge { a, b, len: 0.0 });
                }
            }
        }
        // Структурные: горизонтальные с чётным и нечётным x, затем
        // вертикальные с чётным и нечётным y.
        for parity in 0..2 {
            for y in 0..=r {
                for x in (parity..r).step_by(2) {
                    let a = self.idx(x, y) as u16;
                    let b = self.idx(x + 1, y) as u16;
                    self.structural.push(Edge { a, b, len: 0.0 });
                }
            }
        }
        for parity in 0..2 {
            for y in (parity..r).step_by(2) {
                for x in 0..=r {
                    let a = self.idx(x, y) as u16;
                    let b = self.idx(x, y + 1) as u16;
                    self.structural.push(Edge { a, b, len: 0.0 });
                }
            }
        }
        // Граница по кругу: верх слева направо, правая сторона вниз,
        // низ справа налево, левая сторона вверх.
        for x in 0..r {
            self.boundary.push(self.idx(x, 0) as u16);
        }
        for y in 0..r {
            self.boundary.push(self.idx(r, y) as u16);
        }
        for x in (1..=r).rev() {
            self.boundary.push(self.idx(x, r) as u16);
        }
        for y in (1..=r).rev() {
            self.boundary.push(self.idx(0, y) as u16);
        }
    }

    /// Положение частицы в недеформированном прямоугольнике.
    #[inline]
    fn rect_point(&self, uv: [f32; 2]) -> [f32; 2] {
        [(uv[0] - 0.5) * self.width, (uv[1] - 0.5) * self.height]
    }

    pub fn size(&self) -> (f32, f32) {
        (self.width, self.height)
    }

    pub fn resolution(&self) -> u32 {
        self.res as u32
    }

    /// Окно изменило размер: форма покоя и текущая форма масштабируются
    /// пропорционально (пластическая деформация сохраняется).
    pub fn resize(&mut self, width: f32, height: f32) {
        let width = Self::sane_extent(width);
        let height = Self::sane_extent(height);
        let sx = width / self.width;
        let sy = height / self.height;
        if (sx - 1.0).abs() < 1e-6 && (sy - 1.0).abs() < 1e-6 {
            return;
        }
        self.width = width;
        self.height = height;
        let s = |p: &mut [f32; 2]| {
            p[0] *= sx;
            p[1] *= sy;
        };
        for arr in [
            &mut self.pos,
            &mut self.prev,
            &mut self.rest,
            &mut self.base,
            &mut self.goal,
        ] {
            arr.iter_mut().for_each(s);
        }
        for g in &mut self.grabs {
            s(&mut g.offset);
            s(&mut g.base_anchor);
            s(&mut g.disp);
        }
        // Радиус ядра зависит от размера — пересчитываем веса.
        for k in 0..self.grabs.len() {
            self.compute_grab_weights(k);
        }
        self.rebuild_targets();
        self.rest_generation += 1;
        self.wake();
    }

    /// Обратно к плоскому прямоугольнику: снимает пластику, движение и захваты.
    pub fn reset(&mut self) {
        for i in 0..self.uv.len() {
            let p = self.rect_point(self.uv[i]);
            self.pos[i] = p;
            self.prev[i] = p;
            self.rest[i] = p;
            self.base[i] = p;
        }
        self.grabs.clear();
        self.rebuild_targets();
        self.rest_generation += 1;
        self.active = false;
        self.settled = true;
        self.support_timer = 0.0;
        self.support_strength = 0.0;
    }

    fn wake(&mut self) {
        self.active = true;
        self.settled = false;
    }

    /// Счётчик изменений формы покоя: вызывающему пора пересобрать коллайдер
    /// по [`rest_hull`](Self::rest_hull), когда значение изменилось.
    pub fn rest_generation(&self) -> u64 {
        self.rest_generation
    }

    pub fn has_grabs(&self) -> bool {
        !self.grabs.is_empty()
    }

    /// Пересобирает целевую форму, форму покоя, удержание и длины/площади
    /// ограничений по текущим захватам. Вызывается не из `step`.
    fn rebuild_targets(&mut self) {
        let n = self.pos.len();
        if self.grabs.is_empty() {
            self.goal.copy_from_slice(&self.rest);
            self.hold.iter_mut().for_each(|h| *h = 0.0);
        } else {
            self.rest.copy_from_slice(&self.base);
            self.goal.copy_from_slice(&self.base);
            self.hold.iter_mut().for_each(|h| *h = 0.0);
            for g in &self.grabs {
                let elastic = g.plastic + g.compliance * (1.0 - g.plastic);
                for i in 0..n {
                    let w = g.weights[i];
                    if w <= 0.0 {
                        continue;
                    }
                    self.rest[i] = add(self.rest[i], scale(g.disp, g.plastic * w));
                    self.goal[i] = add(self.goal[i], scale(g.disp, elastic * w));
                    if elastic > 0.0 {
                        let w2 = w * w;
                        self.hold[i] = self.hold[i].max(w2 * w2);
                    }
                }
            }
        }
        for i in 0..n {
            // Полностью удерживаемая частица — бесконечная масса.
            self.inv_mass[i] = (1.0 - self.hold[i]).max(0.0);
        }
        for e in self.structural.iter_mut().chain(self.shear.iter_mut()) {
            e.len = len(sub(self.goal[e.b as usize], self.goal[e.a as usize]));
        }
        for c in &mut self.cells {
            c.area = Self::quad_area(&self.goal, c.q);
        }
    }

    #[inline]
    fn quad_area(p: &[[f32; 2]], q: [u16; 4]) -> f32 {
        let d1 = sub(p[q[2] as usize], p[q[0] as usize]);
        let d2 = sub(p[q[3] as usize], p[q[1] as usize]);
        0.5 * cross(d1, d2)
    }

    /// Один шаг симуляции.
    ///
    /// `lin_accel_local` — ускорение жёсткой системы отсчёта (тела окна) в её
    /// локальных осях, px/с²; частицы чувствуют противоположную инерционную
    /// псевдосилу, поэтому жёсткая посадка сплющивает желе, и оно колышется
    /// обратно. Чтобы окно ещё и чуть проседало под собственным весом в покое,
    /// передавайте «собственное» ускорение `a − g` (то, что мерил бы
    /// акселерометр); с чистым `a` в покое проседания нет.
    /// `ang_vel` — угловая скорость тела (рад/с), даёт центробежное растяжение.
    pub fn step(&mut self, dt: f32, lin_accel_local: [f32; 2], ang_vel: f32, params: &SoftParams) {
        let softness = sanitize(params.softness).clamp(0.0, 1.0);
        let squash = sanitize(params.impact_squash).clamp(0.0, 1.0);

        // Жёсткое окно: сетка в точности равна целевой форме.
        if softness <= 1e-4 {
            self.pos.copy_from_slice(&self.goal);
            self.prev.copy_from_slice(&self.goal);
            self.active = false;
            self.settled = true;
            return;
        }

        let dt = sanitize(dt).clamp(0.0, 0.1);
        if dt <= 0.0 {
            return;
        }
        let extent = self.width.max(self.height);
        let accel = scale(sanitize2(lin_accel_local), -squash);
        let omega = sanitize(ang_vel).clamp(-50.0, 50.0);
        let centrifugal = omega * omega * squash;

        // Спим: сетка успокоилась, а внешних сил нет.
        let force_small = len(accel) < 1.0 && centrifugal * extent < 1.0;
        if self.settled && force_small {
            self.active = false;
            return;
        }

        let substeps = ((dt / BASE_DT).ceil() as usize).clamp(1, MAX_SUBSTEPS);
        let h = dt / substeps as f32;
        let ratio = h / BASE_DT;

        // Параметры материала из softness (экспоненциально: ощущается ровнее).
        let anchor60 = ANCHOR_MIN.powf(softness);
        let anchor = 1.0 - (1.0 - anchor60).powf(ratio);
        let damp60 = DAMP_RIGID + (DAMP_JELLY - DAMP_RIGID) * softness;
        let keep = (1.0 - damp60).powf(ratio);
        let k_struct = 1.0 + (0.35 - 1.0) * softness;
        let k_shear = 0.5 * k_struct;
        let k_area = 1.0 + (0.5 - 1.0) * softness;

        // Ограничения от «взрыва» при экстремальном вводе.
        let max_dv = 3.0 * extent; // px/с прироста скорости за подшаг
        let max_step = 0.25 * extent; // px смещения за подшаг
        let max_dev = 0.8 * extent; // px отклонения от цели

        // Вес псевдосилы: 0 у опорной грани (куда давит сила), 1 у дальней.
        let f_len = len(accel);
        let has_linear = f_len >= 1.0;
        if has_linear {
            let dir = scale(accel, 1.0 / f_len);
            let (s_min, s_max) = Self::extent_along(&self.goal, dir);
            let span = (s_max - s_min).max(1e-3);
            for (w, g) in self.support.iter_mut().zip(&self.goal) {
                *w = ((s_max - dot(*g, dir)) / span).clamp(0.0, 1.0);
            }
            // Опорная плоскость: новая сила заменяет старую, если она того же
            // направления или сравнима по величине с ещё не забытым ударом
            // (мелкий шум контактов rapier не сбивает опору после посадки).
            let same_dir = dot(dir, self.support_dir) > 0.95;
            if same_dir || f_len >= 0.5 * self.support_strength {
                self.support_strength = if same_dir {
                    self.support_strength.max(f_len)
                } else {
                    f_len
                };
                self.support_dir = dir;
                self.support_timer = SUPPORT_MEMORY;
            }
        }
        self.support_strength *= 0.5f32.powf(dt / SUPPORT_HALF_LIFE);
        let plane = (self.support_timer > 0.0).then(|| {
            let (_, s_max) = Self::extent_along(&self.goal, self.support_dir);
            (self.support_dir, s_max)
        });
        // Сетка не выходит за свою целевую форму: без захвата это форма
        // покоя, по которой построен коллайдер окна.
        let hull = convex_hull(&self.goal);
        let mut touched = false;
        let dv = clamp_len(scale(accel, h), max_dv);
        let lin_disp = scale(dv, h);

        let n = self.pos.len();
        for _ in 0..substeps {
            // Интегрирование Verlet с затуханием.
            for i in 0..n {
                let p = self.pos[i];
                let mut v = scale(sub(p, self.prev[i]), keep);
                if has_linear {
                    v = add(v, scale(lin_disp, self.support[i]));
                }
                if centrifugal > 0.0 {
                    v = add(v, scale(p, centrifugal * h * h));
                }
                v = clamp_len(v, max_step);
                self.prev[i] = p;
                self.pos[i] = add(p, v);
            }

            // Якорные пружины к целевой форме (удерживаемые — жёстко).
            for i in 0..n {
                let a = anchor + (1.0 - anchor) * self.hold[i];
                let d = sub(self.goal[i], self.pos[i]);
                self.pos[i] = add(self.pos[i], scale(d, a));
            }

            for _ in 0..ITERATIONS {
                Self::solve_edges(&mut self.pos, &self.inv_mass, &self.structural, k_struct);
                Self::solve_edges(&mut self.pos, &self.inv_mass, &self.shear, k_shear);
                Self::solve_cells(&mut self.pos, &self.inv_mass, &self.cells, k_area);
                // Опора (пол/препятствие, в которое давит сила) не пускает
                // сетку за грань формы покоя: импульс гасится о неё, как о пол.
                if let Some((dir, s_max)) = plane {
                    for p in self.pos.iter_mut() {
                        let s = dot(*p, dir);
                        if s > s_max {
                            *p = sub(*p, scale(dir, s - s_max));
                            touched = true;
                        }
                    }
                }
                for p in self.pos.iter_mut() {
                    if let Some(q) = project_inside(&hull, *p) {
                        *p = q;
                    }
                }
            }

            // Страховка: не уходить слишком далеко от цели.
            for i in 0..n {
                let d = sub(self.pos[i], self.goal[i]);
                if dot(d, d) > max_dev * max_dev {
                    self.pos[i] = add(self.goal[i], clamp_len(d, max_dev));
                }
            }
        }
        // Пока сетка прижата к опоре, контакт «помнится».
        if touched {
            self.support_timer = SUPPORT_MEMORY;
        } else {
            self.support_timer -= dt;
        }

        // Активность и NaN-страховка.
        let mut max_speed2 = 0.0f32;
        let mut max_dev2 = 0.0f32;
        let mut broken = false;
        for i in 0..n {
            let p = self.pos[i];
            if !finite2(p) {
                broken = true;
                break;
            }
            let v = sub(p, self.prev[i]);
            max_speed2 = max_speed2.max(dot(v, v));
            let d = sub(p, self.goal[i]);
            max_dev2 = max_dev2.max(dot(d, d));
        }
        if broken {
            self.pos.copy_from_slice(&self.goal);
            self.prev.copy_from_slice(&self.goal);
            self.active = false;
            self.settled = true;
            self.support_timer = 0.0;
            self.support_strength = 0.0;
            return;
        }
        let speed = max_speed2.sqrt() / h;
        self.active = speed > ACTIVE_SPEED;
        if !self.active && force_small && max_dev2 < 0.05 * 0.05 {
            // Докатываем до точной цели, чтобы не было вечного дрожания.
            self.pos.copy_from_slice(&self.goal);
            self.prev.copy_from_slice(&self.goal);
            self.settled = true;
            self.support_timer = 0.0;
            self.support_strength = 0.0;
        } else {
            self.settled = false;
        }
    }

    /// Диапазон проекций точек на направление `dir`.
    #[inline]
    fn extent_along(p: &[[f32; 2]], dir: [f32; 2]) -> (f32, f32) {
        p.iter().fold((f32::MAX, f32::MIN), |(lo, hi), g| {
            let s = dot(*g, dir);
            (lo.min(s), hi.max(s))
        })
    }

    /// Проекция ограничений длины (Гаусс—Зейдель).
    #[inline]
    fn solve_edges(pos: &mut [[f32; 2]], inv: &[f32], edges: &[Edge], k: f32) {
        for e in edges {
            let (a, b) = (e.a as usize, e.b as usize);
            let wa = inv[a];
            let wb = inv[b];
            let wsum = wa + wb;
            if wsum < 1e-6 {
                continue;
            }
            let d = sub(pos[b], pos[a]);
            let l = len(d);
            if l < 1e-6 {
                continue;
            }
            let c = (l - e.len) / (l * wsum) * k;
            pos[a] = add(pos[a], scale(d, c * wa));
            pos[b] = sub(pos[b], scale(d, c * wb));
        }
    }

    /// Проекция ограничений площади ячеек: сохраняет «объём» желе —
    /// сплющенное окно раздаётся вбок, насколько позволяет его форма.
    #[inline]
    fn solve_cells(pos: &mut [[f32; 2]], inv: &[f32], cells: &[Cell], k: f32) {
        for c in cells {
            let [i0, i1, i2, i3] = c.q.map(|i| i as usize);
            let d1 = sub(pos[i2], pos[i0]);
            let d2 = sub(pos[i3], pos[i1]);
            let area = 0.5 * cross(d1, d2);
            // Градиенты площади по углам.
            let g2 = [0.5 * d2[1], -0.5 * d2[0]];
            let g3 = [-0.5 * d1[1], 0.5 * d1[0]];
            let n02 = dot(g2, g2);
            let n13 = dot(g3, g3);
            let denom = (inv[i0] + inv[i2]) * n02 + (inv[i1] + inv[i3]) * n13;
            if denom < 1e-9 {
                continue;
            }
            let lambda = -(area - c.area) / denom * k;
            pos[i0] = sub(pos[i0], scale(g2, lambda * inv[i0]));
            pos[i2] = add(pos[i2], scale(g2, lambda * inv[i2]));
            pos[i1] = sub(pos[i1], scale(g3, lambda * inv[i1]));
            pos[i3] = add(pos[i3], scale(g3, lambda * inv[i3]));
        }
    }

    /// Радиус спада влияния захвата.
    fn falloff_radius(&self) -> f32 {
        let lo = self.width.min(self.height);
        let hi = self.width.max(self.height);
        (0.6 * (self.width * self.height).sqrt()).clamp(0.5 * lo, hi)
    }

    fn compute_grab_weights(&mut self, k: usize) {
        let r = self.falloff_radius();
        let n = self.pos.len();
        let g = &mut self.grabs[k];
        g.weights.resize(n, 0.0);
        for i in 0..n {
            g.weights[i] = falloff(len(sub(self.base[i], g.base_anchor)), r);
        }
    }

    /// Захват точки сетки под курсором `local`. Если точка внутри сетки — берём
    /// ровно ту точку материала, что под курсором; если снаружи, но не дальше
    /// `radius` от края — ближайшую точку края. Иначе `None`.
    pub fn grab(&mut self, local: [f32; 2], radius: f32) -> Option<SoftGrab> {
        if !finite2(local) {
            return None;
        }
        let radius = sanitize(radius).max(0.0);
        let (anchor_uv, material) = if let Some(uv) = self.local_to_uv(local, false) {
            (uv, self.uv_to_local(uv))
        } else {
            let mut best: Option<(f32, [f32; 2], [f32; 2])> = None;
            let nb = self.boundary.len();
            for k in 0..nb {
                let a = self.boundary[k] as usize;
                let b = self.boundary[(k + 1) % nb] as usize;
                let (t, d2) = closest_on_segment(local, self.pos[a], self.pos[b]);
                if best.is_none_or(|(bd, _, _)| d2 < bd) {
                    let uv = add(self.uv[a], scale(sub(self.uv[b], self.uv[a]), t));
                    let p = add(self.pos[a], scale(sub(self.pos[b], self.pos[a]), t));
                    best = Some((d2, uv, p));
                }
            }
            let (d2, uv, p) = best?;
            if d2.sqrt() > radius {
                return None;
            }
            (uv, p)
        };

        if self.grabs.is_empty() {
            self.base.copy_from_slice(&self.rest);
        }
        let base_anchor = Self::map_uv(&self.base, self.res, anchor_uv);
        let id = self.next_grab_id;
        self.next_grab_id = self.next_grab_id.wrapping_add(1).max(1);
        self.grabs.push(GrabState {
            id,
            anchor_uv,
            offset: sub(local, material),
            base_anchor,
            // Ловим желе там, где оно сейчас.
            disp: sub(material, base_anchor),
            plastic: 0.0,
            compliance: 1.0,
            weights: Vec::new(),
        });
        let k = self.grabs.len() - 1;
        self.compute_grab_weights(k);
        self.rebuild_targets();
        self.wake();
        Some(SoftGrab { id })
    }

    /// Тянет захваченную точку к `local_target` (со спадом на соседей).
    /// Пластичная доля смещения сразу переносится в форму покоя: после
    /// отпускания окно вернётся не к прямоугольнику, а к изменённой форме.
    pub fn drag(&mut self, grab: &SoftGrab, local_target: [f32; 2], params: &SoftParams) {
        if !finite2(local_target) {
            return;
        }
        let Some(k) = self.grabs.iter().position(|g| g.id == grab.id) else {
            return;
        };
        let plastic = sanitize(params.plasticity).clamp(0.0, 1.0);
        let comp = compliance(sanitize(params.softness));
        let extent = self.width.max(self.height);
        {
            let g = &mut self.grabs[k];
            g.plastic = plastic;
            g.compliance = comp;
            let material = sub(local_target, g.offset);
            // Не даём утащить точку бесконечно далеко.
            g.disp = clamp_len(sub(material, g.base_anchor), 2.0 * extent);
        }
        // Якорь целевой формы должен попасть ровно под курсор (с учётом
        // податливости). Ядро спада внутри ячейки не равно 1, и соседние захваты
        // мешают — пара итераций поправки на невязку.
        let factor = plastic + comp * (1.0 - plastic);
        if factor > 1e-3 {
            let wanted = {
                let g = &self.grabs[k];
                add(g.base_anchor, scale(g.disp, factor))
            };
            for _ in 0..3 {
                self.rebuild_targets();
                let got = Self::map_uv(&self.goal, self.res, self.grabs[k].anchor_uv);
                let fix = scale(sub(wanted, got), 1.0 / factor);
                let g = &mut self.grabs[k];
                g.disp = clamp_len(add(g.disp, fix), 2.0 * extent);
            }
        }
        self.rebuild_targets();
        if plastic > 0.0 {
            self.rest_generation += 1;
        }
        self.wake();
    }

    /// Отпускает захват: пластичная часть навсегда остаётся в форме покоя,
    /// упругая — отпружинивает.
    pub fn release(&mut self, grab: SoftGrab) {
        let Some(k) = self.grabs.iter().position(|g| g.id == grab.id) else {
            return;
        };
        let g = self.grabs.remove(k);
        if g.plastic > 0.0 {
            for (b, w) in self.base.iter_mut().zip(&g.weights) {
                *b = add(*b, scale(g.disp, g.plastic * w));
            }
        }
        if self.grabs.is_empty() {
            self.rest.copy_from_slice(&self.base);
        }
        self.rebuild_targets();
        self.rest_generation += 1;
        self.wake();
    }

    /// Форма покоя или текущая форма отличается от прямоугольника > ~0.5 px.
    pub fn is_deformed(&self) -> bool {
        let eps2 = DEFORMED_EPS * DEFORMED_EPS;
        self.uv.iter().enumerate().any(|(i, uv)| {
            let r = self.rect_point(*uv);
            let dr = sub(self.rest[i], r);
            let dp = sub(self.pos[i], r);
            dot(dr, dr) > eps2 || dot(dp, dp) > eps2
        })
    }

    /// Сетка ещё заметно движется (вызывающий продолжает перерисовку).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Текущие положения частиц, локальные px.
    pub fn positions(&self) -> &[[f32; 2]] {
        &self.pos
    }

    /// Форма покоя (с пластикой), локальные px.
    pub fn rest_positions(&self) -> &[[f32; 2]] {
        &self.rest
    }

    /// UV частиц, 0..1, начало слева сверху.
    pub fn uvs(&self) -> &[[f32; 2]] {
        &self.uv
    }

    /// Список треугольников (по 2 на ячейку). Порядок важен: при наложении
    /// (смятое окно) позже идущий треугольник рисуется поверх, и обратная
    /// проекция выбирает тоже его.
    pub fn indices(&self) -> &[u16] {
        &self.indices
    }

    /// Точка массива положений `p` по uv: линейно внутри того же треугольника
    /// ячейки, что рисует GPU. Вне 0..1 — линейная экстраполяция крайней ячейки.
    fn map_uv(p: &[[f32; 2]], res: usize, uv: [f32; 2]) -> [f32; 2] {
        let uv = sanitize2(uv);
        let r = res as f32;
        let fx = uv[0] * r;
        let fy = uv[1] * r;
        let cx = (fx.floor().max(0.0) as usize).min(res - 1);
        let cy = (fy.floor().max(0.0) as usize).min(res - 1);
        let s = fx - cx as f32;
        let t = fy - cy as f32;
        let stride = res + 1;
        let tl = p[cy * stride + cx];
        let tr = p[cy * stride + cx + 1];
        let bl = p[(cy + 1) * stride + cx];
        let br = p[(cy + 1) * stride + cx + 1];
        let mix3 = |a: [f32; 2], wa: f32, b: [f32; 2], wb: f32, c: [f32; 2], wc: f32| {
            [
                a[0] * wa + b[0] * wb + c[0] * wc,
                a[1] * wa + b[1] * wb + c[1] * wc,
            ]
        };
        if (cx + cy).is_multiple_of(2) {
            if s >= t {
                mix3(tl, 1.0 - s, tr, s - t, br, t)
            } else {
                mix3(tl, 1.0 - t, br, s, bl, t - s)
            }
        } else if s + t <= 1.0 {
            mix3(tl, 1.0 - s - t, tr, s, bl, t)
        } else {
            mix3(tr, 1.0 - t, br, s + t - 1.0, bl, 1.0 - s)
        }
    }

    /// Прямое отображение uv (0..1 плоского буфера) → локальная точка текущей
    /// (деформированной) сетки. Линейно внутри треугольника — так же, как
    /// рисует GPU, поэтому [`local_to_uv`](Self::local_to_uv) — точная обратная.
    pub fn uv_to_local(&self, uv: [f32; 2]) -> [f32; 2] {
        Self::map_uv(&self.pos, self.res, uv)
    }

    /// Обратная проекция (§6): локальная экранная точка на деформированной
    /// сетке → uv (0..1) плоского буфера. Ищет содержащий треугольник по
    /// барицентрическим координатам; при наложении треугольников берёт верхний
    /// (последний по порядку отрисовки). Вне сетки — `None`, а при
    /// `extrapolate` — линейная экстраполяция ближайшего треугольника (нужно,
    /// пока пользователь тянет выделение текста за пределы окна; uv тогда может
    /// выйти за 0..1).
    pub fn local_to_uv(&self, local: [f32; 2], extrapolate: bool) -> Option<[f32; 2]> {
        if !finite2(local) {
            return None;
        }
        const EPS: f32 = 1e-4;
        let mut found: Option<[f32; 2]> = None;
        for tri in self.indices.chunks_exact(3) {
            let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            let (pa, pb, pc) = (self.pos[a], self.pos[b], self.pos[c]);
            // Быстрый отсев по bbox треугольника.
            let min_x = pa[0].min(pb[0]).min(pc[0]) - 0.01;
            let max_x = pa[0].max(pb[0]).max(pc[0]) + 0.01;
            let min_y = pa[1].min(pb[1]).min(pc[1]) - 0.01;
            let max_y = pa[1].max(pb[1]).max(pc[1]) + 0.01;
            if local[0] < min_x || local[0] > max_x || local[1] < min_y || local[1] > max_y {
                continue;
            }
            let Some(w) = barycentric(local, pa, pb, pc) else {
                continue;
            };
            if w[0] >= -EPS && w[1] >= -EPS && w[2] >= -EPS {
                found = Some(self.mix_uv(a, b, c, w));
            }
        }
        if found.is_some() || !extrapolate {
            return found;
        }

        // Снаружи: ближайший невырожденный треугольник, барицентрики без
        // ограничения знака = линейное продолжение.
        let mut best: Option<(f32, usize)> = None;
        for (t, tri) in self.indices.chunks_exact(3).enumerate() {
            let (pa, pb, pc) = (
                self.pos[tri[0] as usize],
                self.pos[tri[1] as usize],
                self.pos[tri[2] as usize],
            );
            if cross(sub(pb, pa), sub(pc, pa)).abs() < 1e-6 {
                continue;
            }
            let d2 = closest_on_segment(local, pa, pb)
                .1
                .min(closest_on_segment(local, pb, pc).1)
                .min(closest_on_segment(local, pc, pa).1);
            if best.is_none_or(|(bd, _)| d2 < bd) {
                best = Some((d2, t));
            }
        }
        if let Some((_, t)) = best {
            let tri = &self.indices[t * 3..t * 3 + 3];
            let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
            if let Some(w) = barycentric(local, self.pos[a], self.pos[b], self.pos[c]) {
                let uv = self.mix_uv(a, b, c, w);
                if finite2(uv) {
                    return Some(uv);
                }
            }
        }
        // Сетка выродилась целиком — отображение недеформированного прямоугольника.
        Some([local[0] / self.width + 0.5, local[1] / self.height + 0.5])
    }

    #[inline]
    fn mix_uv(&self, a: usize, b: usize, c: usize, w: [f32; 3]) -> [f32; 2] {
        let (ua, ub, uc) = (self.uv[a], self.uv[b], self.uv[c]);
        [
            ua[0] * w[0] + ub[0] * w[1] + uc[0] * w[2],
            ua[1] * w[0] + ub[1] * w[1] + uc[1] * w[2],
        ]
    }

    /// Выпуклая оболочка ГРАНИЦЫ формы покоя — для коллайдера физики.
    /// Монотонная цепь Эндрю, без коллинеарных точек; для плоской сетки —
    /// ровно 4 угла. Порядок обхода — по часовой стрелке на экране (y вниз).
    pub fn rest_hull(&self) -> Vec<[f32; 2]> {
        let mut pts: Vec<[f32; 2]> = self
            .boundary
            .iter()
            .map(|&i| self.rest[i as usize])
            .filter(|p| finite2(*p))
            .collect();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4);
        if pts.len() < 3 {
            return pts;
        }
        // Порог коллинеарности относительно размера окна.
        let eps = 1e-4 * self.width.max(self.height);
        let turn = |o: [f32; 2], a: [f32; 2], b: [f32; 2]| cross(sub(a, o), sub(b, o));
        let mut hull: Vec<[f32; 2]> = Vec::with_capacity(pts.len() + 1);
        for &p in &pts {
            while hull.len() >= 2 && turn(hull[hull.len() - 2], hull[hull.len() - 1], p) <= eps {
                hull.pop();
            }
            hull.push(p);
        }
        let lower = hull.len() + 1;
        for &p in pts.iter().rev().skip(1) {
            while hull.len() >= lower && turn(hull[hull.len() - 2], hull[hull.len() - 1], p) <= eps
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
        hull
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    fn jelly() -> SoftParams {
        SoftParams {
            softness: 0.8,
            impact_squash: 1.0,
            plasticity: 0.0,
        }
    }

    fn max_dev_from_rect(m: &SoftMesh) -> f32 {
        m.uvs()
            .iter()
            .zip(m.positions())
            .map(|(uv, p)| len(sub(*p, m.rect_point(*uv))))
            .fold(0.0, f32::max)
    }

    fn assert_finite(m: &SoftMesh) {
        for p in m.positions() {
            assert!(finite2(*p), "NaN/inf в положениях: {p:?}");
        }
        for p in m.rest_positions() {
            assert!(finite2(*p), "NaN/inf в форме покоя: {p:?}");
        }
    }

    #[test]
    fn rigid_mesh_equals_rectangle() {
        let mut m = SoftMesh::new(400.0, 300.0, 8);
        let p = SoftParams {
            softness: 0.0,
            impact_squash: 1.0,
            plasticity: 0.0,
        };
        for i in 0..120 {
            let a = if i % 10 == 0 {
                [5.0e4, -9.0e4]
            } else {
                [0.0, 980.0]
            };
            m.step(DT, a, 7.0, &p);
        }
        // Перетаскивание жёсткого окна без пластики тоже ничего не меняет.
        let g = m.grab([200.0, 0.0], 5.0).expect("край в пределах радиуса");
        m.drag(&g, [300.0, 40.0], &p);
        m.step(DT, [0.0, 0.0], 0.0, &p);
        m.release(g);
        m.step(DT, [0.0, 0.0], 0.0, &p);
        for (uv, pos) in m.uvs().iter().zip(m.positions()) {
            let r = m.rect_point(*uv);
            assert_eq!(*pos, r);
        }
        assert!(!m.is_deformed());
        assert!(!m.is_active());
        let hull = m.rest_hull();
        assert_eq!(hull.len(), 4, "оболочка прямоугольника — 4 угла: {hull:?}");
        for c in [
            [-200.0, -150.0],
            [200.0, -150.0],
            [200.0, 150.0],
            [-200.0, 150.0],
        ] {
            assert!(
                hull.iter().any(|h| len(sub(*h, c)) < 1e-3),
                "нет угла {c:?}"
            );
        }
    }

    #[test]
    fn uv_round_trip_on_deformed_mesh() {
        let mut m = SoftMesh::new(500.0, 320.0, 12);
        let params = jelly();
        // Деформируем: тянем угол и трясём.
        let g = m.grab([250.0, 160.0], 4.0).expect("угол");
        for i in 0..20 {
            let t = i as f32 / 20.0;
            m.drag(&g, [250.0 + 90.0 * t, 160.0 + 60.0 * t], &params);
            m.step(DT, [0.0, if i == 5 { -6.0e4 } else { 0.0 }], 1.5, &params);
        }
        assert!(
            max_dev_from_rect(&m) > 20.0,
            "сетка должна быть заметно деформирована"
        );

        let mut checked = 0;
        for iy in 0..=23 {
            for ix in 0..=23 {
                let uv = [ix as f32 / 23.0, iy as f32 / 23.0];
                let local = m.uv_to_local(uv);
                let back = m
                    .local_to_uv(local, false)
                    .unwrap_or_else(|| panic!("точка сетки {local:?} (uv {uv:?}) не найдена"));
                assert!(
                    (back[0] - uv[0]).abs() < 1e-3 && (back[1] - uv[1]).abs() < 1e-3,
                    "uv {uv:?} -> {local:?} -> {back:?}"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 24 * 24);

        // И в обратную сторону: local -> uv -> local.
        for &p in &[[0.0, 0.0], [100.0, -50.0], [-180.0, 120.0]] {
            if let Some(uv) = m.local_to_uv(p, false) {
                let q = m.uv_to_local(uv);
                assert!(len(sub(p, q)) < 1e-2, "{p:?} -> {uv:?} -> {q:?}");
            }
        }
    }

    #[test]
    fn extrapolation_outside_mesh() {
        let m = SoftMesh::new(400.0, 200.0, 6);
        // Внутри — точное отображение прямоугольника.
        let uv = m.local_to_uv([100.0, 50.0], false).expect("внутри");
        assert!((uv[0] - 0.75).abs() < 1e-5 && (uv[1] - 0.75).abs() < 1e-5);
        // Снаружи справа на 40 px.
        let out = [240.0, 0.0];
        assert!(m.local_to_uv(out, false).is_none());
        let uv = m.local_to_uv(out, true).expect("экстраполяция");
        assert!((uv[0] - 1.1).abs() < 1e-4, "{uv:?}");
        assert!((uv[1] - 0.5).abs() < 1e-4, "{uv:?}");
        // Снаружи по диагонали сверху слева.
        let uv = m
            .local_to_uv([-260.0, -130.0], true)
            .expect("экстраполяция");
        assert!(
            (uv[0] + 0.15).abs() < 1e-4 && (uv[1] + 0.15).abs() < 1e-4,
            "{uv:?}"
        );
        // Прямое отображение экстраполирует так же.
        let p = m.uv_to_local([1.1, 0.5]);
        assert!(len(sub(p, out)) < 1e-3, "{p:?}");
    }

    #[test]
    fn impact_squash_recovers() {
        let mut m = SoftMesh::new(400.0, 300.0, 10);
        let params = jelly();
        // Падение со скоростью 1500 px/с и остановка за один кадр:
        // ускорение тела вверх (−y), псевдосила вниз — опора у нижней грани.
        m.step(DT, [0.0, -1500.0 / DT], 0.0, &params);
        let mut max_top_drop = 0.0f32;
        let mut max_bottom_drop = 0.0f32;
        let mut max_width = 0.0f32;
        let top = m.idx(5, 0);
        let bottom = m.idx(5, 10);
        let left = m.idx(0, 5);
        let right = m.idx(10, 5);
        let mut t = 0.0;
        let mut settled_at = None;
        while t < 6.0 {
            m.step(DT, [0.0, 0.0], 0.0, &params);
            t += DT;
            let p = m.positions();
            max_top_drop = max_top_drop.max(p[top][1] + 150.0);
            max_bottom_drop = max_bottom_drop.max((p[bottom][1] - 150.0).abs());
            max_width = max_width.max(p[right][0] - p[left][0]);
            if settled_at.is_none() && !m.is_active() {
                settled_at = Some(t);
            }
        }
        assert_finite(&m);
        assert!(
            max_top_drop > 15.0,
            "верх должен заметно просесть: {max_top_drop}"
        );
        assert!(
            max_bottom_drop < 0.5 * max_top_drop,
            "низ (опора) почти на месте: {max_bottom_drop} vs {max_top_drop}"
        );
        assert!(
            max_width <= 400.01,
            "окно не выходит за свою форму (коллайдер): {max_width}"
        );
        let settled_at = settled_at.expect("сетка должна успокоиться");
        assert!(settled_at < 4.0, "успокоилась за {settled_at} с");
        assert!(max_dev_from_rect(&m) < 0.5);
        assert!(!m.is_deformed());
    }

    #[test]
    fn mesh_stays_inside_its_shape() {
        // Удары во все стороны и вращение: желе колышется, но не вылезает
        // за прямоугольник 400×300 — за коллайдер окна.
        let mut m = SoftMesh::new(400.0, 300.0, 10);
        let params = jelly();
        let hits = [[0.0, -9e4], [9e4, 0.0], [-6e4, 6e4], [0.0, 9e4]];
        for (k, hit) in hits.iter().enumerate() {
            m.step(DT, *hit, if k % 2 == 0 { 12.0 } else { -12.0 }, &params);
            for _ in 0..20 {
                m.step(DT, [0.0, 0.0], 3.0, &params);
                for p in m.positions() {
                    assert!(
                        p[0].abs() <= 200.01 && p[1].abs() <= 150.01,
                        "частица {p:?} за формой окна"
                    );
                }
            }
        }
        assert_finite(&m);
    }

    #[test]
    fn load_filter_forgets_constant_load() {
        let mut f = LoadFilter::default();
        // Первое значение — опорное: рывка нет.
        assert_eq!(f.apply([0.0, -1500.0], DT), [0.0, 0.0]);
        // Лежит на полу: вес не сминает окно.
        for _ in 0..120 {
            let out = f.apply([0.0, -1500.0], DT);
            assert!(out[1].abs() < 1e-3);
        }
        // Удар — сразу целиком, потом забывается.
        let hit = f.apply([0.0, -90_000.0], DT);
        assert!((hit[1] + 88_500.0).abs() < 1.0, "{hit:?}");
        let mut out = hit;
        for _ in 0..60 {
            out = f.apply([0.0, -90_000.0], DT);
        }
        assert!(out[1].abs() < 0.05 * 88_500.0, "{out:?}");
    }

    #[test]
    fn drag_with_plasticity_persists_and_reset_clears() {
        let mut m = SoftMesh::new(400.0, 300.0, 10);
        let params = SoftParams {
            softness: 0.7,
            impact_squash: 0.5,
            plasticity: 0.5,
        };
        // Берём середину правого края (чуть снаружи — в пределах радиуса).
        let g = m.grab([203.0, 0.0], 6.0).expect("край");
        for i in 1..=30 {
            m.drag(&g, [203.0 + 100.0 * i as f32 / 30.0, 0.0], &params);
            m.step(DT, [0.0, 0.0], 0.0, &params);
        }
        // Точка захвата держится под курсором.
        let right_mid = m.idx(10, 5);
        let p = m.positions()[right_mid];
        assert!((p[0] - 300.0).abs() < 2.0 && p[1].abs() < 2.0, "{p:?}");
        let gen_before = m.rest_generation();
        m.release(g);
        assert!(m.rest_generation() > gen_before);
        for _ in 0..(6.0 / DT) as usize {
            m.step(DT, [0.0, 0.0], 0.0, &params);
        }
        assert!(!m.is_active());
        assert!(m.is_deformed(), "пластика оставляет форму");
        let p = m.positions()[right_mid];
        assert!(
            (p[0] - 250.0).abs() < 3.0,
            "половина из 100 px осталась: {p:?}"
        );
        // Левый край не тронут (вне радиуса спада почти ничего).
        let left_mid = m.idx(0, 5);
        assert!((m.positions()[left_mid][0] + 200.0).abs() < 10.0);
        // Оболочка покоя выросла вправо.
        let hull = m.rest_hull();
        let max_x = hull.iter().map(|h| h[0]).fold(f32::MIN, f32::max);
        assert!((max_x - 250.0).abs() < 3.0, "{max_x}");

        m.reset();
        assert!(!m.is_deformed());
        assert_eq!(max_dev_from_rect(&m), 0.0);
        // Старый хэндл после reset безвреден.
        m.drag(&SoftGrab { id: 1 }, [500.0, 500.0], &params);
        assert!(!m.is_deformed());
    }

    #[test]
    fn elastic_drag_springs_back() {
        let mut m = SoftMesh::new(300.0, 300.0, 8);
        let params = jelly();
        let g = m.grab([-150.0, -150.0], 3.0).expect("угол");
        m.drag(&g, [-220.0, -200.0], &params);
        for _ in 0..10 {
            m.step(DT, [0.0, 0.0], 0.0, &params);
        }
        let corner = m.positions()[0];
        assert!(len(sub(corner, [-220.0, -200.0])) < 1.0, "{corner:?}");
        m.release(g);
        for _ in 0..(5.0 / DT) as usize {
            m.step(DT, [0.0, 0.0], 0.0, &params);
        }
        assert!(!m.is_deformed());
        assert!(!m.is_active());
    }

    #[test]
    fn resize_scales_shape() {
        let mut m = SoftMesh::new(200.0, 100.0, 4);
        let params = SoftParams {
            softness: 0.5,
            impact_squash: 0.0,
            plasticity: 1.0,
        };
        let g = m.grab([100.0, 0.0], 2.0).expect("край");
        m.drag(&g, [140.0, 0.0], &params);
        m.release(g);
        m.step(DT, [0.0, 0.0], 0.0, &params);
        let i = m.idx(4, 2);
        let before = m.rest_positions()[i];
        assert!((before[0] - 140.0).abs() < 1.0, "{before:?}");
        m.resize(400.0, 50.0);
        assert_eq!(m.size(), (400.0, 50.0));
        let after = m.rest_positions()[i];
        assert!((after[0] - 2.0 * before[0]).abs() < 1e-3, "{after:?}");
        let corner = m.rest_positions()[0];
        assert!(len(sub(corner, [-200.0, -25.0])) < 1e-3, "{corner:?}");
    }

    #[test]
    fn no_nans_under_extreme_input() {
        let mut m = SoftMesh::new(640.0, 480.0, 16);
        let params = SoftParams {
            softness: 1.0,
            impact_squash: 1.0,
            plasticity: 0.3,
        };
        let crazy = [
            [f32::NAN, 0.0],
            [f32::INFINITY, -f32::INFINITY],
            [1e30, -1e30],
            [3e6, 0.0],
            [0.0, -3e6],
        ];
        let g = m.grab([0.0, 0.0], 1.0).expect("центр");
        for i in 0..400 {
            let a = crazy[i % crazy.len()];
            let w = if i % 7 == 0 {
                f32::NAN
            } else {
                1e4 * (i as f32).sin()
            };
            let dt = match i % 9 {
                0 => f32::NAN,
                1 => 5.0,
                2 => -1.0,
                _ => DT,
            };
            m.step(dt, a, w, &params);
            let target = if i % 13 == 0 {
                [f32::NAN, 1.0]
            } else {
                [1e7 * (i as f32).cos(), 1e7 * (i as f32).sin()]
            };
            m.drag(&g, target, &params);
            assert_finite(&m);
        }
        m.release(g);
        let bad = SoftParams {
            softness: f32::NAN,
            impact_squash: f32::INFINITY,
            plasticity: -3.0,
        };
        for _ in 0..50 {
            m.step(DT, [1e9, 1e9], 1e9, &bad);
            m.step(DT, [1e9, -1e9], -1e9, &params);
        }
        assert_finite(&m);
        let extent = 640.0f32;
        for p in m.positions() {
            assert!(
                p[0].abs() < 10.0 * extent && p[1].abs() < 10.0 * extent,
                "{p:?}"
            );
        }
        assert!(m.local_to_uv([f32::NAN, 0.0], true).is_none());
        let uv = m.local_to_uv([1e6, 1e6], true).expect("экстраполяция");
        assert!(finite2(uv));
        assert!(finite2(m.uv_to_local([f32::NAN, 2.0])));
        assert!(m.rest_hull().iter().all(|p| finite2(*p)));
        // Вырожденные размеры.
        let mut tiny = SoftMesh::new(0.0, f32::NAN, 0);
        tiny.step(DT, [1e5, 1e5], 3.0, &params);
        assert_finite(&tiny);
        assert_eq!(tiny.resolution(), 1);
        let big = SoftMesh::new(10.0, 10.0, 1000);
        assert_eq!(big.resolution(), 32);
        assert!(big.positions().len() <= u16::MAX as usize);
    }

    #[test]
    fn indices_cover_grid() {
        let m = SoftMesh::new(100.0, 100.0, 5);
        assert_eq!(m.indices().len(), 5 * 5 * 6);
        assert_eq!(m.positions().len(), 36);
        assert!(
            m.indices()
                .iter()
                .all(|&i| (i as usize) < m.positions().len())
        );
        // Все треугольники одной ориентации (по часовой на экране).
        for tri in m.indices().chunks_exact(3) {
            let p = m.positions();
            let (a, b, c) = (p[tri[0] as usize], p[tri[1] as usize], p[tri[2] as usize]);
            assert!(cross(sub(b, a), sub(c, a)) > 0.0);
        }
        // Суммарная площадь треугольников = площадь прямоугольника.
        let area: f32 = m
            .indices()
            .chunks_exact(3)
            .map(|t| {
                let p = m.positions();
                0.5 * cross(
                    sub(p[t[1] as usize], p[t[0] as usize]),
                    sub(p[t[2] as usize], p[t[0] as usize]),
                )
            })
            .sum();
        assert!((area - 10000.0).abs() < 1e-2);
    }

    /// Замер стоимости шага: `cargo test --release softbody::tests::bench_step -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_step() {
        let params = jelly();
        for res in [8, 16, 32] {
            let mut m = SoftMesh::new(800.0, 600.0, res);
            let g = m.grab([400.0, 0.0], 5.0).expect("край");
            m.drag(&g, [480.0, 30.0], &params);
            m.release(g);
            let n = 20_000;
            let start = std::time::Instant::now();
            for i in 0..n {
                // Постоянная сила (как «собственное» ускорение в покое) не даёт
                // уснуть — меряем полный шаг; периодические удары раскачивают сетку.
                let a = if i % 120 == 0 {
                    [0.0, -9.0e4]
                } else {
                    [0.0, -980.0]
                };
                m.step(DT, a, 0.3, &params);
            }
            let per = start.elapsed().as_secs_f64() / n as f64 * 1e6;
            println!("SoftMesh {res}x{res}: {per:.2} мкс/шаг");
        }
        // Обратная проекция (на каждое движение курсора).
        let mut m = SoftMesh::new(800.0, 600.0, 16);
        m.step(DT, [0.0, -9.0e4], 0.3, &params);
        let n = 20_000;
        let start = std::time::Instant::now();
        let mut acc = 0.0f32;
        for i in 0..n {
            let p = [(i % 800) as f32 - 400.0, (i % 600) as f32 - 300.0];
            acc += m.local_to_uv(p, true).map_or(0.0, |uv| uv[0]);
        }
        let per = start.elapsed().as_secs_f64() / n as f64 * 1e6;
        println!("local_to_uv 16x16: {per:.2} мкс/запрос ({acc})");
    }
}
