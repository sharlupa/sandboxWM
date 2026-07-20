//! Конфигурация sandboxWM.
//!
//! Файлы конфига лежат в отдельной директории и имеют расширение `.conf`
//! (как в Hyprland) — внутри это обычный TOML. Ищется директория (первая найденная):
//! 1. `$SANDBOXWM_CONFIG` (если это директория)
//! 2. `$XDG_CONFIG_HOME/sandboxWM/`
//! 3. `~/.config/sandboxWM/`
//! 4. `./config/` (рядом с cwd)
//!
//! Внутри найденной директории читаются все файлы `*.conf` (по алфавиту, для
//! стабильного порядка) и склеиваются в один TOML-документ. Каждый файл отвечает за
//! свои секции:
//!   - `sandboxWM.conf`  — `[physics]` и `[controls]` (реализовано, работает)
//!   - `gravity.conf`    — `[gravity_modes]`, `[planned_controls]` (заготовка)
//!   - `drawing.conf`    — `[drawing]`, `[materials.*]` (заготовка)
//!   - `joints.conf`     — `[joints.*]` (заготовка)
//!   - `softbody.conf`   — `[soft_body]` (заготовка)
//!   - `slicing.conf`    — `[slicing]` (заготовка)
//!   - `session.conf`    — `[session]` (заготовка)
//!   - `window.conf`     — `[window_props]` (заготовка)
//!   - `rendering.conf`  — `[rendering]` (заготовка)
//!
//! Секции-заготовки уже содержат разумные дефолты по концепту
//! (`sandboxWM_concept.md`), но ни одна из них пока не читается кодом — это
//! заготовки под будущую реализацию, а не рабочие настройки.
//!
//! Отсутствующие файлы/поля → [`Default`].

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Корневой конфиг.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub physics: PhysicsConfig,
    pub controls: ControlsConfig,

    // ── Заглушки (фици из концепта, ещё не реализованы) ────────────
    /// Режимы гравитации (концепт §1, режимы 2–4). Сейчас есть только общий
    /// тумблер тайлинг↔физика (`controls.toggle_physics`); заморозка в
    /// тайлинг, невесомость одного окна и полная невесомость — план.
    pub gravity_modes: GravityModesConfig,
    /// Клавиши для функций из концепта, которых пока нет в коде.
    pub planned_controls: PlannedControlsConfig,
    /// Рисование физических линий курсором.
    pub drawing: DrawingConfig,
    /// Материалы линий: сталь / клей / батут / верёвка.
    pub materials: MaterialsConfig,
    /// Скрепление окон (Weld / Spring / Rope).
    pub joints: JointsConfig,
    /// Мягкие тела / деформация окон.
    pub soft_body: SoftBodyConfig,
    /// Разрезание окон (слайсинг).
    pub slicing: SlicingConfig,
    /// Персистентность сессии (сохранение/восстановление).
    pub session: SessionConfig,
    /// Свойства окон (вес, мягкость и т.п. — per-window overrides).
    pub window_props: WindowPropsConfig,
    /// Будущий рендер-бэкенд (сейчас GLES через Smithay; план — wgpu/glow
    /// mesh-рендер для деформации окон).
    pub rendering: RenderingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            physics: PhysicsConfig::default(),
            controls: ControlsConfig::default(),
            gravity_modes: GravityModesConfig::default(),
            planned_controls: PlannedControlsConfig::default(),
            drawing: DrawingConfig::default(),
            materials: MaterialsConfig::default(),
            joints: JointsConfig::default(),
            soft_body: SoftBodyConfig::default(),
            slicing: SlicingConfig::default(),
            session: SessionConfig::default(),
            window_props: WindowPropsConfig::default(),
            rendering: RenderingConfig::default(),
        }
    }
}

impl Config {
    /// Загружает конфиг из директории `.conf`-файлов или возвращает defaults.
    pub fn load() -> Self {
        match Self::find_config_dir() {
            Some(dir) => match Self::load_from_dir(&dir) {
                Ok(cfg) => {
                    println!("[config] загружен из директории: {}", dir.display());
                    cfg
                }
                Err(e) => {
                    eprintln!(
                        "[config] ошибка чтения директории {}: {e} — используем defaults",
                        dir.display()
                    );
                    Self::default()
                }
            },
            None => {
                println!("[config] директория конфига не найдена — используем defaults");
                Self::default()
            }
        }
    }

    /// Считывает и склеивает все `*.conf`-файлы директории (по алфавиту,
    /// для стабильного порядка), затем парсит результат как единый
    /// TOML-документ. Каждый файл отвечает за свои таблицы (`[physics]`,
    /// `[joints.weld]`, …), поэтому конкатенация безопасна, если файлы не описывают
    /// одну и ту же таблицу дважды.
    fn load_from_dir(dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path.extension().and_then(|ext| ext.to_str()) == Some("conf")
            })
            .collect();
        files.sort();

        let mut combined = String::new();
        for path in &files {
            combined.push_str(&std::fs::read_to_string(path)?);
            combined.push('\n');
        }

        let cfg: Config = toml::from_str(&combined)?;
        Ok(cfg)
    }

    fn find_config_dir() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("SANDBOXWM_CONFIG") {
            let pb = PathBuf::from(p);
            if pb.is_dir() {
                return Some(pb);
            }
        }

        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            let p = PathBuf::from(xdg).join("sandboxWM");
            if p.is_dir() {
                return Some(p);
            }
        }

        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home).join(".config").join("sandboxWM");
            if p.is_dir() {
                return Some(p);
            }
        }

        let local = PathBuf::from("config");
        if local.is_dir() {
            return Some(local);
        }

        None
    }
}

// ── Реализованные секции ───────────────────────────────────────────

/// Параметры физического мира и тел окон.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PhysicsConfig {
    /// Шаг симуляции в секундах (обычно `1/60 ≈ 0.0166667`).
    pub physics_dt: f32,
    /// Гравитация по Y (px/s²). Logical Y растёт вниз → положительная.
    pub gravity_y: f32,
    /// Мировая Y-координата пола.
    pub floor_y: f32,
    /// Полуширина пола (cuboid half-extent по X).
    pub floor_half_w: f32,
    /// Полутолщина пола (cuboid half-extent по Y).
    pub floor_half_h: f32,
    /// Плотность коллайдера окна (kg/px²); масса ∝ площади.
    pub density: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub friction: f32,
    pub restitution: f32,
    /// Лимит скорости при drag мышью (px/s).
    pub max_drag_speed: f32,
    /// Угловая скорость при удержании spin-клавиш (рад/с).
    pub spin_rate: f32,
    /// Шаг камеры по стрелкам (px).
    pub camera_step: f64,
    /// Коэффициент lerp камеры за кадр (0..1).
    pub camera_lerp: f64,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            physics_dt: 1.0 / 60.0,
            gravity_y: 1500.0,
            floor_y: 2000.0,
            floor_half_w: 50_000.0,
            floor_half_h: 100.0,
            density: 0.002,
            linear_damping: 1.5,
            angular_damping: 3.5,
            friction: 0.85,
            restitution: 0.2,
            max_drag_speed: 12000.0,
            spin_rate: 2.0,
            camera_step: 120.0,
            camera_lerp: 0.15,
        }
    }
}

/// Горячие клавиши и связанные команды.
///
/// Имена клавиш — как в XKB без префикса `KEY_`: `Return`, `Escape`, `a`,
/// `Left`, `F1`, … Сравнение case-insensitive для букв.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlsConfig {
    /// Команда терминала для spawn (Super+Enter по умолчанию).
    pub terminal: String,
    pub spawn_terminal: String,
    pub quit: Vec<String>,
    pub close_window: String,
    pub toggle_physics: String,
    pub spin_left: String,
    pub spin_right: String,
    pub camera_left: String,
    pub camera_right: String,
    pub camera_up: String,
    pub camera_down: String,
}

impl Default for ControlsConfig {
    fn default() -> Self {
        Self {
            terminal: "kitty".into(),
            spawn_terminal: "Return".into(),
            quit: vec!["q".into(), "Escape".into()],
            close_window: "w".into(),
            toggle_physics: "g".into(),
            spin_left: "a".into(),
            spin_right: "d".into(),
            camera_left: "Left".into(),
            camera_right: "Right".into(),
            camera_up: "Up".into(),
            camera_down: "Down".into(),
        }
    }
}

impl ControlsConfig {
    /// Совпадает ли keysym (u32 XKB) с именем из конфига.
    pub fn matches_key(name: &str, keysym: u32) -> bool {
        keysym_name(keysym)
            .map(|n| n.eq_ignore_ascii_case(name.trim()))
            .unwrap_or(false)
    }

    pub fn is_quit(&self, keysym: u32) -> bool {
        self.quit.iter().any(|k| Self::matches_key(k, keysym))
    }
}

/// Имя клавиши без префикса `KEY_` для известных биндов; иначе `None`.
fn keysym_name(keysym: u32) -> Option<&'static str> {
    use smithay::input::keyboard::keysyms as xkb;
    // Буквы a–z (и A–Z) → одна каноническая строка в нижнем регистре.
    const LETTERS: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    if (xkb::KEY_a..=xkb::KEY_z).contains(&keysym) {
        return Some(LETTERS[(keysym - xkb::KEY_a) as usize]);
    }
    if (xkb::KEY_A..=xkb::KEY_Z).contains(&keysym) {
        return Some(LETTERS[(keysym - xkb::KEY_A) as usize]);
    }
    Some(match keysym {
        xkb::KEY_Return => "Return",
        xkb::KEY_Escape => "Escape",
        xkb::KEY_space => "space",
        xkb::KEY_Tab => "Tab",
        xkb::KEY_Left => "Left",
        xkb::KEY_Right => "Right",
        xkb::KEY_Up => "Up",
        xkb::KEY_Down => "Down",
        xkb::KEY_F1 => "F1",
        xkb::KEY_F2 => "F2",
        xkb::KEY_F3 => "F3",
        xkb::KEY_F4 => "F4",
        xkb::KEY_F5 => "F5",
        xkb::KEY_F6 => "F6",
        xkb::KEY_F7 => "F7",
        xkb::KEY_F8 => "F8",
        xkb::KEY_F9 => "F9",
        xkb::KEY_F10 => "F10",
        xkb::KEY_F11 => "F11",
        xkb::KEY_F12 => "F12",
        xkb::KEY_0 => "0",
        xkb::KEY_1 => "1",
        xkb::KEY_2 => "2",
        xkb::KEY_3 => "3",
        xkb::KEY_4 => "4",
        xkb::KEY_5 => "5",
        xkb::KEY_6 => "6",
        xkb::KEY_7 => "7",
        xkb::KEY_8 => "8",
        xkb::KEY_9 => "9",
        _ => return None,
    })
}

// ── Заглушки будущих фиц ──────────────────────────────────────────
// Поля ниже уже несут разумные дефолты по описанию из `sandboxWM_concept.md`,
// но ни `state.rs`, ни `physics.rs`, ни `input.rs` их пока не читают. Ети
// подготовленная поверхность конфигурации — реализация подключит логику к
// этим полям постепенно, по одной фиче.

/// Режимы гравитации (концепт §1). `physics.gravity_y` — режим 1 (стандарт).
/// Здесь параметры для режимов 2–4, которых пока нет в `physics.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GravityModesConfig {
    /// Режим 2: длительность плавного перехода "физика → строгий тайлинг", сек.
    pub freeze_transition_secs: f32,
    /// Порог скорости тела, ниже которого окно считается "улёгшимся" перед
    /// заморозкой в тайлинг (px/s).
    pub freeze_settle_speed: f32,
    /// Режим 4: гравитация во время полной невесомости (обычно 0.0).
    pub weightless_gravity_y: f32,
}

impl Default for GravityModesConfig {
    fn default() -> Self {
        Self {
            freeze_transition_secs: 0.35,
            freeze_settle_speed: 5.0,
            weightless_gravity_y: 0.0,
        }
    }
}

/// Клавиши для функций из концепта, которых пока нет в `input.rs`.
/// Раздел отдельный от [`ControlsConfig`], чтобы не путать уже рабочие
/// биндинги с заготовками.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlannedControlsConfig {
    /// Режим 2: заморозить гравитацию → строгий тайлинг.
    pub gravity_freeze: String,
    /// Режим 3: невесомость только для сфокусированного окна.
    pub gravity_float_window: String,
    /// Режим 4: невесомость для всех окон.
    pub gravity_zero: String,
    /// Включить инструмент рисования физических линий (концепт §2).
    pub draw_line: String,
    /// Включить инструмент "нож" — слайсинг окна (концепт §7).
    pub slice_window: String,
    /// Сварить (WeldJoint) два выделенных окна (концепт §4).
    pub weld_windows: String,
    /// Соединить окна пружиной (SpringJoint) (концепт §4).
    pub spring_windows: String,
    /// Соединить окна верёвкой (концепт §4).
    pub rope_windows: String,
}

impl Default for PlannedControlsConfig {
    fn default() -> Self {
        Self {
            gravity_freeze: "2".into(),
            gravity_float_window: "3".into(),
            gravity_zero: "4".into(),
            draw_line: "p".into(),
            slice_window: "x".into(),
            weld_windows: "e".into(),
            spring_windows: "s".into(),
            rope_windows: "r".into(),
        }
    }
}

/// Рисование линий курсором (концепт §2). Пока не используется.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DrawingConfig {
    /// Минимальное расстояние между соседними точками линии перед добавлением
    /// нового сегмента-коллайдера (px). Сглаживает дрожание мыши.
    pub min_point_spacing: f32,
    /// Толщина коллайдера линии по умолчанию (px), если материал её не переопределяет.
    pub default_thickness: f32,
    /// Максимальное число точек в одной нарисованной линии.
    pub max_points: u32,
    /// Цвет превью линии во время рисования, до применения материала (hex).
    pub preview_color: String,
}

impl Default for DrawingConfig {
    fn default() -> Self {
        Self {
            min_point_spacing: 8.0,
            default_thickness: 12.0,
            max_points: 512,
            preview_color: "#ffffff".into(),
        }
    }
}

/// Материалы нарисованных линий (концепт §2): сталь / клей / батут / верёвка.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MaterialsConfig {
    pub steel: SteelMaterialConfig,
    pub glue: GlueMaterialConfig,
    pub trampoline: TrampolineMaterialConfig,
    pub rope: RopeMaterialConfig,
}

/// Сталь (пол/полка) — обычная твёрдая линия.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SteelMaterialConfig {
    pub friction: f32,
    pub restitution: f32,
    pub thickness: f32,
    pub color: String,
}

impl Default for SteelMaterialConfig {
    fn default() -> Self {
        Self {
            friction: 0.8,
            restitution: 0.05,
            thickness: 12.0,
            color: "#8a8f98".into(),
        }
    }
}

/// Клей — высокое `friction`; `detach_force` — усилие для отрыва окна.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlueMaterialConfig {
    pub friction: f32,
    pub restitution: f32,
    pub thickness: f32,
    pub color: String,
    /// Сила drag'а (px/s условной скорости отрыва), нужная чтобы оторвать
    /// приклеенное окно.
    pub detach_force: f32,
}

impl Default for GlueMaterialConfig {
    fn default() -> Self {
        Self {
            friction: 5.0,
            restitution: 0.0,
            thickness: 10.0,
            color: "#c9a227".into(),
            detach_force: 2000.0,
        }
    }
}

/// Батут — высокое `restitution`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrampolineMaterialConfig {
    pub friction: f32,
    pub restitution: f32,
    pub thickness: f32,
    pub color: String,
}

impl Default for TrampolineMaterialConfig {
    fn default() -> Self {
        Self {
            friction: 0.3,
            restitution: 1.4,
            thickness: 10.0,
            color: "#3aa1ff".into(),
        }
    }
}

/// Верёвка — цепочка коротких тел, соединённых `BallJoint`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RopeMaterialConfig {
    pub segment_length: f32,
    pub segment_count: u32,
    pub thickness: f32,
    pub color: String,
    pub joint_stiffness: f32,
}

impl Default for RopeMaterialConfig {
    fn default() -> Self {
        Self {
            segment_length: 20.0,
            segment_count: 16,
            thickness: 6.0,
            color: "#7a5230".into(),
            joint_stiffness: 1.0,
        }
    }
}

/// Joints между окнами (концепт §4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JointsConfig {
    pub weld: WeldJointConfig,
    pub spring: SpringJointConfig,
    pub rope: RopeJointConfig,
}

/// Сварка (`WeldJoint`) — монолит из двух окон.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeldJointConfig {
    /// Сила удара, при которой сварной шов разрушается. `0.0` = неразрушимый.
    pub break_force: f32,
}

impl Default for WeldJointConfig {
    fn default() -> Self {
        Self { break_force: 0.0 }
    }
}

/// Пружина (`SpringJoint`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpringJointConfig {
    pub stiffness: f32,
    pub damping: f32,
    pub rest_length: f32,
}

impl Default for SpringJointConfig {
    fn default() -> Self {
        Self {
            stiffness: 200.0,
            damping: 10.0,
            rest_length: 150.0,
        }
    }
}

/// Верёвка между окнами. По умолчанию проходит сквозь скреплённые окна
/// (как описано в концепте).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RopeJointConfig {
    pub segment_length: f32,
    pub max_length: f32,
    pub collides_with_linked_windows: bool,
}

impl Default for RopeJointConfig {
    fn default() -> Self {
        Self {
            segment_length: 20.0,
            max_length: 600.0,
            collides_with_linked_windows: false,
        }
    }
}

/// Мягкие тела / деформация окон (концепт §5–6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftBodyConfig {
    /// Степень мягкости: `0.0` — жёсткое окно, `1.0` — желе.
    pub softness: f32,
    /// Насколько сильно окно сплющивается при сильном ударе об пол.
    pub impact_squash: f32,
    /// Число сегментов сетки (mesh) по каждой стороне окна.
    pub mesh_resolution: u32,
    /// Обратное проецирование курсора для выделения текста на искривлённой
    /// сетке (концепт §6).
    pub text_reprojection: bool,
}

impl Default for SoftBodyConfig {
    fn default() -> Self {
        Self {
            softness: 0.0,
            impact_squash: 0.0,
            mesh_resolution: 8,
            text_reprojection: false,
        }
    }
}

/// Слайсинг окон (концепт §7, `wp_viewporter`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlicingConfig {
    /// Минимальный размер получившегося кусочка после разреза (px).
    pub min_piece_size: f32,
    /// Визуальный зазор между разрезанными частями (px).
    pub cut_gap: f32,
}

impl Default for SlicingConfig {
    fn default() -> Self {
        Self {
            min_piece_size: 80.0,
            cut_gap: 4.0,
        }
    }
}

/// Сохранение/восстановление сессии (концепт §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub enabled: bool,
    pub autosave: bool,
    pub autosave_interval_secs: u32,
    /// Пусто = использовать XDG state dir по умолчанию, когда фича появится.
    pub save_path: String,
    /// `"toml"` или `"json"`.
    pub format: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            autosave: false,
            autosave_interval_secs: 60,
            save_path: String::new(),
            format: "toml".into(),
        }
    }
}

/// Per-window overrides свойств (вес, мягкость и т.д.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPropsConfig {
    /// Множитель плотности/веса по умолчанию для новых окон.
    pub default_weight: f32,
    /// Степень мягкости по умолчанию (см. [`SoftBodyConfig`]).
    pub default_softness: f32,
    /// Разрешить переопределять свойства конкретных окон индивидуально.
    pub allow_overrides: bool,
}

impl Default for WindowPropsConfig {
    fn default() -> Self {
        Self {
            default_weight: 1.0,
            default_softness: 0.0,
            allow_overrides: true,
        }
    }
}

/// Будущий рендер-бэкенд. Сейчас окна рисуются через GLES (Smithay);
/// `wgpu`/`glow` понадобится для mesh-рендера деформируемых окон.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingConfig {
    /// `"gles"` (текущий бэкенд) или `"wgpu"` / `"glow"` (план).
    pub backend: String,
    pub vsync: bool,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            backend: "gles".into(),
            vsync: true,
        }
    }
}
