//! Конфигурация sandboxWM.
//!
//! Файлы конфига лежат в отдельной директории и имеют расширение `.conf`
//! (как в Hyprland) — внутри это обычный TOML. Ищется директория (первая найденная):
//! 1. `$SANDBOXWM_CONFIG` (если это директория)
//! 2. `$XDG_CONFIG_HOME/sandboxWM/`
//! 3. `~/.config/sandboxWM/`
//! 4. `./config/` (рядом с cwd)
//!
//! Внутри найденной директории читаются все файлы `*.conf` (по алфавиту) и
//! сливаются потаблично: одинаковые таблицы из разных файлов объединяются,
//! повторный ключ из более позднего файла перекрывает ранний. Раньше файлы
//! склеивались строкой, и любая повторная секция (`[gravity_modes]` была и в
//! `gravity.conf`, и в `sandboxWM.conf`) давала `duplicate key` — весь конфиг
//! молча откатывался к дефолтам. Файл с синтаксической ошибкой теперь
//! пропускается с сообщением, остальные применяются.
//!
//! Раскладка по файлам (рекомендуемая, не обязательная):
//!   - `sandboxWM.conf`  — `[physics]`, `[controls]`
//!   - `gravity.conf`    — `[gravity_modes]`
//!   - `drawing.conf`    — `[drawing]`, `[materials.*]`
//!   - `joints.conf`     — `[joints.*]`
//!   - `softbody.conf`   — `[soft_body]`
//!   - `slicing.conf`    — `[slicing]`
//!   - `session.conf`    — `[session]`
//!   - `window.conf`     — `[window_props]`
//!   - `rendering.conf`  — `[rendering]`
//!
//! Отсутствующие файлы/поля → [`Default`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Корневой конфиг.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub physics: PhysicsConfig,
    pub controls: ControlsConfig,
    /// Режимы гравитации (концепт §1).
    pub gravity_modes: GravityModesConfig,
    /// Рисование физических линий курсором (концепт §2).
    pub drawing: DrawingConfig,
    /// Материалы линий: сталь / клей / батут / верёвка.
    pub materials: MaterialsConfig,
    /// Скрепление окон (Weld / Spring / Rope, концепт §4).
    pub joints: JointsConfig,
    /// Мягкие тела / деформация окон (концепт §5–6).
    pub soft_body: SoftBodyConfig,
    /// Разрезание окон (концепт §7).
    pub slicing: SlicingConfig,
    /// Персистентность сессии (концепт §3).
    pub session: SessionConfig,
    /// Свойства окон по умолчанию и per-app overrides.
    pub window_props: WindowPropsConfig,
    pub rendering: RenderingConfig,
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

    /// Читает все `*.conf` директории и сливает их потаблично (см. модульную
    /// документацию). Ошибка в одном файле не отменяет остальные.
    fn load_from_dir(dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("conf")
            })
            .collect();
        files.sort();

        let mut merged = toml::Table::new();
        for path in &files {
            let text = std::fs::read_to_string(path)?;
            match text.parse::<toml::Table>() {
                Ok(table) => merge_tables(&mut merged, table),
                Err(e) => eprintln!("[config] {} пропущен: {e}", path.display()),
            }
        }

        Ok(toml::Value::Table(merged).try_into()?)
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

    /// Свойства конкретного окна: дефолты + override по app_id/классу.
    pub fn window_override(&self, app_id: &str) -> WindowOverride {
        let props = &self.window_props;
        let mut out = WindowOverride {
            weight: Some(props.default_weight),
            softness: Some(props.default_softness),
            floating: Some(false),
        };
        if props.allow_overrides && !app_id.is_empty() {
            let key = app_id.to_ascii_lowercase();
            if let Some(o) = props
                .apps
                .iter()
                .find(|(k, _)| k.to_ascii_lowercase() == key)
                .map(|(_, v)| v)
            {
                out.weight = o.weight.or(out.weight);
                out.softness = o.softness.or(out.softness);
                out.floating = o.floating.or(out.floating);
            }
        }
        out
    }
}

/// Рекурсивное слияние: таблицы объединяются, прочие значения перекрываются.
fn merge_tables(dst: &mut toml::Table, src: toml::Table) {
    for (key, value) in src {
        match (dst.get_mut(&key), value) {
            (Some(toml::Value::Table(d)), toml::Value::Table(s)) => merge_tables(d, s),
            (_, v) => {
                dst.insert(key, v);
            }
        }
    }
}

/// `#rrggbb` / `#rrggbbaa` → линейный RGBA 0..1. Некорректная строка → белый.
pub fn parse_color(s: &str) -> [f32; 4] {
    let hex = s.trim().trim_start_matches('#');
    let byte = |i: usize| {
        hex.get(i..i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .map(|b| b as f32 / 255.0)
    };
    match hex.len() {
        6 | 8 => {
            let a = if hex.len() == 8 { byte(6) } else { Some(1.0) };
            match (byte(0), byte(2), byte(4), a) {
                (Some(r), Some(g), Some(b), Some(a)) => [r, g, b, a],
                _ => [1.0; 4],
            }
        }
        _ => [1.0; 4],
    }
}

// ── Физика ─────────────────────────────────────────────────────────

/// Параметры физического мира и тел окон.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PhysicsConfig {
    /// Шаг симуляции в секундах (обычно `1/60 ≈ 0.0166667`).
    pub physics_dt: f32,
    /// Гравитация по Y (px/s²). Logical Y растёт вниз → положительная.
    pub gravity_y: f32,
    /// Мировая Y-координата верхней грани пола. Не задано → нижний край
    /// экрана в момент включения физики (раньше дефолт 2000 при экране
    /// 1080 px уводил все окна за кадр).
    pub floor_y: Option<f32>,
    /// Полуширина пола (cuboid half-extent по X).
    pub floor_half_w: f32,
    /// Полутолщина пола (cuboid half-extent по Y).
    pub floor_half_h: f32,
    /// Рисовать линию пола.
    pub floor_visible: bool,
    pub floor_color: String,
    /// Плотность коллайдера окна (масса ∝ площади).
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
    /// Границы зума камеры.
    pub zoom_min: f64,
    pub zoom_max: f64,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            physics_dt: 1.0 / 60.0,
            gravity_y: 1500.0,
            floor_y: None,
            floor_half_w: 50_000.0,
            floor_half_h: 100.0,
            floor_visible: true,
            floor_color: "#cccccc".into(),
            density: 0.002,
            linear_damping: 1.5,
            angular_damping: 3.5,
            friction: 0.85,
            restitution: 0.2,
            max_drag_speed: 12000.0,
            spin_rate: 2.0,
            camera_step: 120.0,
            camera_lerp: 0.15,
            zoom_min: 0.1,
            zoom_max: 3.0,
        }
    }
}

// ── Управление ─────────────────────────────────────────────────────

/// Горячие клавиши. Все действия — с модификатором Super (Logo), кроме
/// клавиш внутри активного инструмента (см. README).
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
    /// Тайлинг ↔ физика (плавный переход в обе стороны).
    pub toggle_physics: String,
    pub spin_left: String,
    pub spin_right: String,
    pub camera_left: String,
    pub camera_right: String,
    pub camera_up: String,
    pub camera_down: String,

    // Режимы гравитации (концепт §1).
    /// Режим 1: обычная гравитация (включает физику, если был тайлинг).
    pub gravity_normal: String,
    /// Режим 2: гравитация выключена → окна плавно встают в тайлинг.
    pub gravity_tiling: String,
    /// Режим 3: невесомость только для активного окна (переключатель).
    pub float_window: String,
    /// Режим 4: невесомость для всех окон.
    pub gravity_zero: String,

    // Инструменты (физический режим).
    /// Рисование линий (концепт §2).
    pub draw_tool: String,
    /// Нож — разрезание окна (концепт §7).
    pub knife_tool: String,
    /// Сварка двух окон (концепт §4).
    pub weld_windows: String,
    /// Пружина между окнами.
    pub spring_windows: String,
    /// Верёвка между окнами.
    pub rope_windows: String,
    /// Снять все связи/крепления активного окна.
    pub unlink_window: String,
    /// Сделать активное окно мягким/жёстким (концепт §5).
    pub toggle_soft: String,
    /// Вернуть форму окна и склеить разрезанные части.
    pub restore_window: String,
    /// Сохранить сессию прямо сейчас.
    pub save_session: String,
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
            gravity_normal: "1".into(),
            gravity_tiling: "2".into(),
            float_window: "3".into(),
            gravity_zero: "4".into(),
            draw_tool: "p".into(),
            knife_tool: "x".into(),
            weld_windows: "e".into(),
            spring_windows: "s".into(),
            rope_windows: "r".into(),
            unlink_window: "u".into(),
            toggle_soft: "j".into(),
            restore_window: "z".into(),
            save_session: "F5".into(),
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
        xkb::KEY_BackSpace => "BackSpace",
        xkb::KEY_Delete => "Delete",
        xkb::KEY_Left => "Left",
        xkb::KEY_Right => "Right",
        xkb::KEY_Up => "Up",
        xkb::KEY_Down => "Down",
        xkb::KEY_minus => "minus",
        xkb::KEY_equal => "equal",
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
        // Цифры и их shifted-варианты на US-раскладке: Super+Shift+1 даёт
        // keysym `exclam`, но пользователь думает о клавише «1».
        xkb::KEY_0 | xkb::KEY_parenright => "0",
        xkb::KEY_1 | xkb::KEY_exclam => "1",
        xkb::KEY_2 | xkb::KEY_at => "2",
        xkb::KEY_3 | xkb::KEY_numbersign => "3",
        xkb::KEY_4 | xkb::KEY_dollar => "4",
        xkb::KEY_5 | xkb::KEY_percent => "5",
        xkb::KEY_6 | xkb::KEY_asciicircum => "6",
        xkb::KEY_7 | xkb::KEY_ampersand => "7",
        xkb::KEY_8 | xkb::KEY_asterisk => "8",
        xkb::KEY_9 | xkb::KEY_parenleft => "9",
        _ => return None,
    })
}

// ── Режимы гравитации ──────────────────────────────────────────────

/// Режимы гравитации (концепт §1). `physics.gravity_y` — режим 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GravityModesConfig {
    /// Режим 2: длительность плавного перехода физика ↔ тайлинг, сек.
    pub freeze_transition_secs: f32,
    /// Порог скорости (px/s), ниже которого окно считается «улёгшимся».
    pub freeze_settle_speed: f32,
    /// Режим 4: гравитация во время полной невесомости (обычно 0.0).
    pub weightless_gravity_y: f32,
    /// Затухание в невесомости: меньше → окна дольше плывут по инерции.
    pub weightless_linear_damping: f32,
}

impl Default for GravityModesConfig {
    fn default() -> Self {
        Self {
            freeze_transition_secs: 0.35,
            freeze_settle_speed: 5.0,
            weightless_gravity_y: 0.0,
            weightless_linear_damping: 0.3,
        }
    }
}

// ── Рисование линий ────────────────────────────────────────────────

/// Рисование линий курсором (концепт §2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DrawingConfig {
    /// Минимальное расстояние между соседними точками линии (px).
    pub min_point_spacing: f32,
    /// Толщина коллайдера, если материал её не задаёт (px).
    pub default_thickness: f32,
    /// Максимальное число точек в одной линии.
    pub max_points: u32,
    /// Цвет превью во время рисования.
    pub preview_color: String,
    /// Материал, выбранный при первом включении инструмента.
    pub default_material: String,
}

impl Default for DrawingConfig {
    fn default() -> Self {
        Self {
            min_point_spacing: 8.0,
            default_thickness: 12.0,
            max_points: 512,
            preview_color: "#ffffff".into(),
            default_material: "steel".into(),
        }
    }
}

/// Материалы нарисованных линий (концепт §2).
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

/// Клей: окно приклеивается при касании и отрывается сильным рывком мышью.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlueMaterialConfig {
    pub friction: f32,
    pub restitution: f32,
    pub thickness: f32,
    pub color: String,
    /// Порог отрыва: изменение скорости окна за шаг (px/s), которое шов
    /// клея должен «погасить», чтобы удержать окно. Сильнее тянешь мышью —
    /// окно отрывается. Собственный вес окна (~25 px/s за шаг) не отрывает.
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

/// Верёвка — цепочка коротких тел на шарнирах.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RopeMaterialConfig {
    /// Длина одного звена (px); нарисованная линия разбивается на звенья.
    pub segment_length: f32,
    /// Максимум звеньев в одной верёвке.
    pub segment_count: u32,
    pub thickness: f32,
    pub color: String,
    /// Жёсткость шарниров 0..1 (1 — нерастяжимая цепь).
    pub joint_stiffness: f32,
    /// Какие концы нарисованной верёвки закреплены: "start", "both", "none".
    pub pin: String,
    /// Свободный конец цепляется за окно, которое к нему поднесли мышью.
    pub hook_windows: bool,
}

impl Default for RopeMaterialConfig {
    fn default() -> Self {
        Self {
            segment_length: 20.0,
            segment_count: 64,
            thickness: 6.0,
            color: "#b07a48".into(),
            joint_stiffness: 1.0,
            pin: "start".into(),
            hook_windows: true,
        }
    }
}

// ── Связи между окнами ─────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JointsConfig {
    pub weld: WeldJointConfig,
    pub spring: SpringJointConfig,
    pub rope: RopeJointConfig,
}

/// Сварка — монолит из двух окон.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WeldJointConfig {
    /// Порог разрушения шва в тех же единицах, что `glue.detach_force`
    /// (px/s за шаг). `0.0` = неразрушимый.
    pub break_force: f32,
}

/// Пружина.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpringJointConfig {
    /// Жёсткость (на единицу массы окна, 1/s²).
    pub stiffness: f32,
    /// Демпфирование (на единицу массы окна, 1/s).
    pub damping: f32,
    /// Длина покоя (px). `0` → текущее расстояние между точками крепления.
    pub rest_length: f32,
    pub color: String,
}

impl Default for SpringJointConfig {
    fn default() -> Self {
        Self {
            stiffness: 200.0,
            damping: 10.0,
            rest_length: 0.0,
            color: "#7fd18b".into(),
        }
    }
}

/// Верёвка между окнами. По умолчанию проходит сквозь скреплённые окна.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RopeJointConfig {
    pub segment_length: f32,
    /// Максимальная длина (px). Верёвка создаётся с длиной = расстояние × 1.1,
    /// но не длиннее этого значения.
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

// ── Мягкие тела ────────────────────────────────────────────────────

/// Мягкие тела / деформация окон (концепт §5–6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftBodyConfig {
    /// Мягкость окна, которое сделали мягким (`toggle_soft` или захват
    /// за край): `0.0` — жёсткое, `1.0` — желе.
    pub softness: f32,
    /// Насколько сильно окно сплющивается при ударе.
    pub impact_squash: f32,
    /// Насколько вытягивание мышью остаётся навсегда (0..1).
    pub plasticity: f32,
    /// Число сегментов сетки по каждой стороне окна.
    pub mesh_resolution: u32,
    /// Обратное проецирование курсора через искривлённую сетку (концепт §6).
    pub text_reprojection: bool,
}

impl Default for SoftBodyConfig {
    fn default() -> Self {
        Self {
            softness: 0.5,
            impact_squash: 0.5,
            plasticity: 0.7,
            mesh_resolution: 10,
            text_reprojection: true,
        }
    }
}

// ── Слайсинг ───────────────────────────────────────────────────────

/// Слайсинг окон (концепт §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlicingConfig {
    /// Минимальный размер получившегося кусочка (px).
    pub min_piece_size: f32,
    /// Расстояние, на которое части расходятся после разреза (px).
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

// ── Сессия ─────────────────────────────────────────────────────────

/// Сохранение/восстановление сессии (концепт §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    pub enabled: bool,
    pub autosave: bool,
    pub autosave_interval_secs: u32,
    /// Пусто = `$XDG_STATE_HOME/sandboxWM/session.<ext>`.
    pub save_path: String,
    /// `"toml"` или `"json"`.
    pub format: String,
    /// Перезапускать приложения, окна которых были открыты.
    pub restore_apps: bool,
    /// Сколько секунд ждать окна перезапущенных приложений.
    pub restore_timeout_secs: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            autosave: true,
            autosave_interval_secs: 60,
            save_path: String::new(),
            format: "toml".into(),
            restore_apps: true,
            restore_timeout_secs: 30,
        }
    }
}

// ── Свойства окон ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowPropsConfig {
    /// Множитель плотности (веса) для новых окон.
    pub default_weight: f32,
    /// Мягкость новых окон (0 — жёсткие).
    pub default_softness: f32,
    /// Разрешить overrides из `[window_props.apps."<app_id>"]`.
    pub allow_overrides: bool,
    /// Свойства конкретных приложений по app_id / X11-классу.
    pub apps: HashMap<String, WindowOverride>,
}

impl Default for WindowPropsConfig {
    fn default() -> Self {
        Self {
            default_weight: 1.0,
            default_softness: 0.0,
            allow_overrides: true,
            apps: HashMap::new(),
        }
    }
}

/// Override свойств окна. Незаданное поле берётся из дефолтов.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowOverride {
    pub weight: Option<f32>,
    pub softness: Option<f32>,
    /// Окно стартует в невесомости (режим 3).
    pub floating: Option<bool>,
}

// ── Рендер ─────────────────────────────────────────────────────────

/// Рендер. Используется GLES через Smithay (деформируемые окна рисуются
/// собственным mesh-шейдером поверх того же GLES-контекста).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderingConfig {
    /// Цвет фона рабочего стола.
    pub background: String,
}

impl Default for RenderingConfig {
    fn default() -> Self {
        Self {
            background: "#14141f".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_sections_across_files_merge() {
        let dir = std::env::temp_dir().join(format!("sandboxwm-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.conf"), "[physics]\ngravity_y = 10.0\n[gravity_modes]\nweightless_gravity_y = 1.0\n").unwrap();
        std::fs::write(dir.join("b.conf"), "[gravity_modes]\nfreeze_transition_secs = 2.0\n[controls]\nterminal = \"foot\"\n").unwrap();
        std::fs::write(dir.join("c.conf"), "this is not toml [[[").unwrap();
        let cfg = Config::load_from_dir(&dir).unwrap();
        assert_eq!(cfg.physics.gravity_y, 10.0);
        assert_eq!(cfg.gravity_modes.weightless_gravity_y, 1.0);
        assert_eq!(cfg.gravity_modes.freeze_transition_secs, 2.0);
        assert_eq!(cfg.controls.terminal, "foot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_config_loads() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
        let cfg = Config::load_from_dir(&dir).unwrap();
        assert_eq!(cfg.controls.toggle_physics, "g");
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff0000"), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(parse_color("00ff0080")[3], 128.0 / 255.0);
        assert_eq!(parse_color("bogus"), [1.0; 4]);
    }

    #[test]
    fn overrides() {
        let mut cfg = Config::default();
        cfg.window_props.apps.insert(
            "Firefox".into(),
            WindowOverride {
                weight: Some(3.0),
                ..Default::default()
            },
        );
        assert_eq!(cfg.window_override("firefox").weight, Some(3.0));
        assert_eq!(cfg.window_override("foot").weight, Some(1.0));
    }
}
