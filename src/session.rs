//! Персистентность сессии (концепт §3).
//!
//! При выходе (и периодически, см. [`Autosave`]) композитор собирает всё рабочее
//! пространство в [`SessionData`] — окна (с кусками после слайсинга), нарисованные
//! линии/верёвки и скрепления — и сохраняет в TOML или JSON ([`save`]). При
//! следующем старте [`load`] читает файл, [`relaunch_commands`] + [`spawn`]
//! перезапускают приложения, а [`SessionRestorer`] сопоставляет появляющиеся окна
//! с сохранёнными, чтобы вернуть им прежние координаты и физические свойства.
//!
//! Модуль намеренно не знает про `AppState`: на входе и выходе — простые данные.
//!
//! Надёжность:
//!   - запись атомарная (временный файл в той же директории + `rename`);
//!   - предыдущая версия файла остаётся в `session.<ext>.bak`;
//!   - битый основной файл → пробуем `.bak`; ничто не паникует на плохих данных;
//!   - после чтения (и перед записью) данные проходят [`validate`]: NaN/inf
//!     выкидываются, абсурдные размеры обрезаются, «висячие» скрепления удаляются.

// Модуль ещё не подключён к композитору (интеграция в state.rs — следующим
// шагом), поэтому часть API пока никем не вызывается.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Текущая версия формата файла сессии.
pub const SESSION_VERSION: u32 = 1;

// ── Пределы валидации ──────────────────────────────────────────────

/// Мировые координаты дальше этого считаем мусором и прижимаем к границе.
const COORD_LIMIT: f32 = 1.0e7;
/// Максимальная сторона окна / куска (пиксели).
const MAX_WINDOW_SIZE: i32 = 16384;
const MIN_ZOOM: f64 = 0.01;
const MAX_ZOOM: f64 = 100.0;
const MIN_WEIGHT: f32 = 0.01;
const MAX_WEIGHT: f32 = 100.0;
const MAX_WINDOWS: usize = 512;
const MAX_PIECES: usize = 64;
const MAX_LINES: usize = 4096;
const MAX_LINE_POINTS: usize = 8192;
const MAX_JOINTS: usize = 4096;
/// Файл больше этого не читаем вовсе (защита от мусора/ошибочного пути).
const MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

// ── Модель данных ──────────────────────────────────────────────────

/// Всё рабочее пространство целиком.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionData {
    /// Версия формата; любая, кроме [`SESSION_VERSION`], не загружается.
    pub version: u32,
    /// `"tiling"` | `"physics"`.
    pub layout_mode: String,
    /// `"normal"` | `"zero"`.
    pub gravity_mode: String,
    /// Камера: смещение x, смещение y, зум.
    pub camera: [f64; 3],
    /// Y физического пола, если он есть.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floor_y: Option<f32>,
    pub windows: Vec<SavedWindow>,
    pub lines: Vec<SavedLine>,
    pub joints: Vec<SavedJoint>,
    /// Рабочий стол этих данных (с нуля). Верхний уровень файла — стол,
    /// указанный здесь (обычно первый), остальные — в `workspaces`.
    pub workspace: usize,
    /// Активный рабочий стол на момент сохранения (только верхний уровень).
    pub active_workspace: usize,
    /// Остальные рабочие столы: у каждого свои окна, линии, связи и камера.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<SessionData>,
}

impl SessionData {
    /// Данные всех столов по отдельности: верхний уровень (без вложенных) и
    /// вложенные `workspaces`.
    pub fn into_workspace_blocks(mut self) -> Vec<SessionData> {
        let nested = std::mem::take(&mut self.workspaces);
        let mut blocks = vec![self];
        blocks.extend(nested.into_iter().map(|mut b| {
            b.workspaces.clear();
            b
        }));
        blocks
    }

    /// Окна и скрепления всех столов одним списком (для сопоставления окон
    /// и перезапуска приложений) + стол каждого окна. Индексы окон в
    /// скреплениях пересчитаны в общий список.
    pub fn merge_workspace_blocks(blocks: &[SessionData]) -> (SessionData, Vec<usize>) {
        let mut merged = SessionData::default();
        let mut workspace_of = Vec::new();
        for block in blocks {
            let offset = merged.windows.len();
            merged.windows.extend(block.windows.iter().cloned());
            workspace_of.extend(std::iter::repeat_n(block.workspace, block.windows.len()));
            merged
                .joints
                .extend(block.joints.iter().cloned().map(|mut j| {
                    j.a.window += offset;
                    j.b.window += offset;
                    j
                }));
        }
        (merged, workspace_of)
    }
}

impl Default for SessionData {
    fn default() -> Self {
        Self {
            // Не 0: файл без поля `version` (или собранный через
            // `..Default::default()`) считается текущей версией.
            version: SESSION_VERSION,
            layout_mode: "physics".into(),
            gravity_mode: "normal".into(),
            camera: [0.0, 0.0, 1.0],
            floor_y: None,
            windows: Vec::new(),
            lines: Vec::new(),
            joints: Vec::new(),
            workspace: 0,
            active_workspace: 0,
            workspaces: Vec::new(),
        }
    }
}

/// Окно приложения.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    /// xdg `app_id` или класс `WM_CLASS` для окон XWayland.
    pub app_id: String,
    pub title: String,
    pub x11: bool,
    /// argv процесса-владельца (из `/proc/<pid>/cmdline`), пусто — неизвестно.
    pub command: Vec<String>,
    /// `/proc/<pid>/cwd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// pid владельца на момент сохранения, 0 — неизвестен. Нужен только
    /// [`relaunch_commands`]: отличить «одно приложение с несколькими окнами»
    /// от «нескольких процессов с одинаковой командой» (терминалы).
    pub pid: u32,
    /// Размер геометрии окна; 0 — неизвестен.
    pub size: [i32; 2],
    /// Множитель плотности.
    pub weight: f32,
    /// Мягкость: `0.0` — жёсткое окно, `1.0` — желе.
    pub softness: f32,
    /// Хотя бы один кусок; больше одного — окно разрезано (концепт §7).
    pub pieces: Vec<SavedPiece>,
}

impl Default for SavedWindow {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            title: String::new(),
            x11: false,
            command: Vec::new(),
            cwd: None,
            pid: 0,
            size: [0, 0],
            weight: 1.0,
            softness: 0.0,
            pieces: Vec::new(),
        }
    }
}

/// Кусок окна = одно физическое тело.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedPiece {
    /// Кадрирование в локальных координатах окна: x, y, w, h.
    pub rect: [f64; 4],
    /// Центр тела в мировых координатах.
    pub center: [f32; 2],
    /// Угол, радианы.
    pub angle: f32,
    /// Невесомость только этого окна (концепт §1, режим 3).
    pub floating: bool,
}

/// Нарисованная линия.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedLine {
    /// `"steel"` | `"glue"` | `"trampoline"` | `"rope"`.
    pub material: String,
    /// Мировые координаты; для верёвки — текущая цепочка сегментов.
    pub points: Vec<[f32; 2]>,
    /// Только для верёвок: закреплены ли концы `[начало, конец]`; может быть пусто.
    pub pinned: Vec<bool>,
}

/// Скрепление двух кусков окон.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedJoint {
    /// `"weld"` | `"spring"` | `"rope"`.
    pub kind: String,
    pub a: SavedAnchor,
    pub b: SavedAnchor,
}

/// Точка крепления скрепления.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedAnchor {
    /// Индекс в [`SessionData::windows`].
    pub window: usize,
    /// Индекс в [`SavedWindow::pieces`].
    pub piece: usize,
    /// Точка в локальных координатах куска (начало — центр куска).
    pub local: [f32; 2],
}

// ── Формат и путь ──────────────────────────────────────────────────

/// Формат файла сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionFormat {
    #[default]
    Toml,
    Json,
}

impl SessionFormat {
    /// Из строки конфига (`[session] format`): `"toml"` / `"json"`.
    /// Неизвестное значение → TOML с предупреждением.
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "toml" | "" => Self::Toml,
            "json" => Self::Json,
            other => {
                eprintln!("[session] неизвестный формат {other:?} — используем toml");
                Self::Toml
            }
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Toml => "toml",
            Self::Json => "json",
        }
    }
}

/// Путь к файлу сессии. Непустой `configured` (из конфига) побеждает, `~/`
/// раскрывается; путь-директория (`…/` или существующая папка) → файл внутри.
/// Иначе `$XDG_STATE_HOME/sandboxWM/session.<ext>`, иначе
/// `~/.local/state/sandboxWM/session.<ext>`.
pub fn session_path(configured: &str, format: SessionFormat) -> PathBuf {
    resolve_session_path(
        configured,
        format,
        env_nonempty("XDG_STATE_HOME").as_deref(),
        env_nonempty("HOME").as_deref(),
    )
}

/// Чистая часть [`session_path`] (без чтения окружения — для тестов).
fn resolve_session_path(
    configured: &str,
    format: SessionFormat,
    xdg_state: Option<&str>,
    home: Option<&str>,
) -> PathBuf {
    let file_name = format!("session.{}", format.extension());

    let configured = configured.trim();
    if !configured.is_empty() {
        let expanded = expand_tilde(configured, home);
        if configured.ends_with('/') || expanded.is_dir() {
            return expanded.join(file_name);
        }
        return expanded;
    }

    // XDG Base Directory: относительный путь в XDG_* надо игнорировать.
    if let Some(state) = xdg_state.filter(|s| Path::new(s).is_absolute()) {
        return Path::new(state).join("sandboxWM").join(file_name);
    }
    if let Some(home) = home {
        return Path::new(home)
            .join(".local")
            .join("state")
            .join("sandboxWM")
            .join(file_name);
    }
    std::env::temp_dir().join("sandboxWM").join(file_name)
}

fn expand_tilde(path: &str, home: Option<&str>) -> PathBuf {
    match home {
        Some(home) if path == "~" => PathBuf::from(home),
        Some(home) => match path.strip_prefix("~/") {
            Some(rest) => Path::new(home).join(rest),
            None => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Путь резервной копии: `session.<ext>` → `session.<ext>.bak`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

// ── Сохранение / загрузка ──────────────────────────────────────────

/// Счётчик для уникальных имён временных файлов внутри процесса.
static TMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Сохраняет сессию атомарно: временный файл в той же директории → `fsync` →
/// `rename`. Предыдущий файл (если он читается) копируется в `.bak`.
/// Данные перед записью проходят [`validate`] (на копии), поэтому NaN не
/// превратится в `null` в JSON и не сломает следующий запуск.
pub fn save(path: &Path, format: SessionFormat, data: &SessionData) -> io::Result<()> {
    let mut clean = data.clone();
    clean.version = SESSION_VERSION;
    validate(&mut clean);
    let text = serialize(&clean, format)?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "путь сессии без имени файла")
    })?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    fs::create_dir_all(&dir)?;

    let tmp = tmp_path(&dir, &file_name.to_string_lossy());
    if let Err(e) = write_synced(&tmp, text.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    backup_previous(path, format);

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // fsync директории, чтобы сам rename пережил сбой питания (не критично).
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Загружает сессию. Нет файла → `None` молча. Битый файл → пробуем `.bak`
/// (с сообщением в stderr), `None`, если не читается и она. Чужая версия
/// формата → предупреждение и `None`. Результат уже прошёл [`validate`].
pub fn load(path: &Path, format: SessionFormat) -> Option<SessionData> {
    let mut data = match read_session(path, format) {
        Ok(data) => data,
        Err(ReadError::Missing) => return None,
        Err(ReadError::Bad(e)) => {
            eprintln!("[session] не удалось прочитать {}: {e}", path.display());
            let bak = backup_path(path);
            match read_session(&bak, format) {
                Ok(data) => {
                    eprintln!("[session] используем резервную копию {}", bak.display());
                    data
                }
                Err(ReadError::Missing) => {
                    eprintln!("[session] резервной копии нет — начинаем с чистого листа");
                    return None;
                }
                Err(ReadError::Bad(e)) => {
                    eprintln!(
                        "[session] резервная копия {} тоже не читается: {e}",
                        bak.display()
                    );
                    return None;
                }
            }
        }
    };

    if data.version != SESSION_VERSION {
        eprintln!(
            "[session] версия файла {} не поддерживается (ожидается {SESSION_VERSION}) — \
             сессия не восстанавливается",
            data.version
        );
        return None;
    }

    let report = validate(&mut data);
    if !report.is_clean() {
        eprintln!("[session] исправлено при загрузке: {report}");
    }
    for (i, ws) in data.workspaces.iter_mut().enumerate() {
        ws.workspaces.clear();
        let report = validate(ws);
        if !report.is_clean() {
            eprintln!(
                "[session] рабочий стол {}: исправлено при загрузке: {report}",
                ws.workspace.max(i + 1) + 1
            );
        }
    }
    Some(data)
}

enum ReadError {
    Missing,
    Bad(String),
}

fn read_session(path: &Path, format: SessionFormat) -> Result<SessionData, ReadError> {
    let text = match read_limited(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) => return Err(ReadError::Bad(e.to_string())),
    };
    parse(&text, format).map_err(ReadError::Bad)
}

fn read_limited(path: &Path) -> io::Result<String> {
    let len = fs::metadata(path)?.len();
    if len > MAX_FILE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("файл слишком большой ({len} байт)"),
        ));
    }
    let bytes = fs::read(path)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn serialize(data: &SessionData, format: SessionFormat) -> io::Result<String> {
    match format {
        // Не `to_string_pretty`: тот разворачивает каждый массив построчно, и
        // верёвка из сотни точек превращается в сотни строк.
        SessionFormat::Toml => toml::to_string(data).map_err(io::Error::other),
        SessionFormat::Json => serde_json::to_string_pretty(data).map_err(io::Error::other),
    }
}

fn parse(text: &str, format: SessionFormat) -> Result<SessionData, String> {
    // Пустой файл — почти наверняка недописанный, а не «пустая сессия».
    if text.trim().is_empty() {
        return Err("файл пуст".into());
    }
    match format {
        SessionFormat::Toml => toml::from_str(text).map_err(|e| e.to_string()),
        SessionFormat::Json => serde_json::from_str(text).map_err(|e| e.to_string()),
    }
}

fn tmp_path(dir: &Path, file_name: &str) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{file_name}.{}.{n}.tmp", std::process::id()))
}

/// Пишет файл целиком и сбрасывает на диск. Права 0600: в сессии лежат
/// командные строки и рабочие директории.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Копирует текущий файл в `.bak` — только если он сам читается, иначе битый
/// файл затёр бы хорошую резервную копию. Ошибки не фатальны для сохранения.
fn backup_previous(path: &Path, format: SessionFormat) {
    let Ok(text) = read_limited(path) else {
        return;
    };
    if parse(&text, format).is_err() {
        eprintln!(
            "[session] {} не читается — резервную копию не обновляем",
            path.display()
        );
        return;
    }
    let bak = backup_path(path);
    let dir = bak.parent().unwrap_or(Path::new("."));
    let name = bak
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = tmp_path(dir, &name);
    if let Err(e) = write_synced(&tmp, text.as_bytes()).and_then(|_| fs::rename(&tmp, &bak)) {
        let _ = fs::remove_file(&tmp);
        eprintln!(
            "[session] не удалось обновить резервную копию {}: {e}",
            bak.display()
        );
    }
}

// ── Валидация ──────────────────────────────────────────────────────

/// Что пришлось выкинуть/исправить при валидации.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub dropped_windows: usize,
    pub dropped_pieces: usize,
    pub dropped_lines: usize,
    pub dropped_points: usize,
    pub dropped_joints: usize,
    /// Значения, заменённые на дефолт или прижатые к пределам.
    pub fixed_values: usize,
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = [
            ("окон удалено", self.dropped_windows),
            ("кусков удалено", self.dropped_pieces),
            ("линий удалено", self.dropped_lines),
            ("точек удалено", self.dropped_points),
            ("скреплений удалено", self.dropped_joints),
            ("значений исправлено", self.fixed_values),
        ];
        let mut first = true;
        for (name, n) in parts.into_iter().filter(|(_, n)| *n > 0) {
            if !first {
                f.write_str(", ")?;
            }
            write!(f, "{name}: {n}")?;
            first = false;
        }
        if first {
            f.write_str("всё в порядке")?;
        }
        Ok(())
    }
}

/// Приводит данные в безопасный вид: выкидывает куски/окна/точки с NaN/inf,
/// линии короче двух точек, скрепления с несуществующими якорями (индексы
/// остальных пересчитываются после удалений), прижимает размеры, зум, вес и
/// мягкость к разумным пределам. Строковые режимы/материалы не трогает —
/// неизвестные значения трактует вызывающий.
pub fn validate(data: &mut SessionData) -> ValidationReport {
    let mut r = ValidationReport::default();

    // Камера
    let [cx, cy, zoom] = data.camera;
    let limit = COORD_LIMIT as f64;
    let camera = [
        if cx.is_finite() {
            cx.clamp(-limit, limit)
        } else {
            0.0
        },
        if cy.is_finite() {
            cy.clamp(-limit, limit)
        } else {
            0.0
        },
        if zoom.is_finite() && zoom > 0.0 {
            zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        } else {
            1.0
        },
    ];
    if camera != data.camera {
        data.camera = camera;
        r.fixed_values += 1;
    }

    if let Some(y) = data.floor_y {
        let fixed = y.is_finite().then(|| clamp_coord(y));
        if fixed != Some(y) {
            data.floor_y = fixed;
            r.fixed_values += 1;
        }
    }

    // Окна. Карты «старый индекс → новый» нужны, чтобы перенацелить скрепления.
    let old_windows = std::mem::take(&mut data.windows);
    let mut window_map: Vec<Option<usize>> = Vec::with_capacity(old_windows.len());
    let mut piece_maps: Vec<Vec<Option<usize>>> = Vec::with_capacity(old_windows.len());
    for mut w in old_windows {
        let mut piece_map = Vec::with_capacity(w.pieces.len());
        if data.windows.len() >= MAX_WINDOWS {
            r.dropped_windows += 1;
            window_map.push(None);
            piece_maps.push(piece_map);
            continue;
        }
        for mut p in std::mem::take(&mut w.pieces) {
            if w.pieces.len() < MAX_PIECES && sanitize_piece(&mut p, &mut r) {
                piece_map.push(Some(w.pieces.len()));
                w.pieces.push(p);
            } else {
                r.dropped_pieces += 1;
                piece_map.push(None);
            }
        }
        piece_maps.push(piece_map);
        // Без единого куска неизвестно, где окно было, — восстанавливать нечего.
        if w.pieces.is_empty() {
            r.dropped_windows += 1;
            window_map.push(None);
            continue;
        }
        sanitize_window_props(&mut w, &mut r);
        window_map.push(Some(data.windows.len()));
        data.windows.push(w);
    }

    // Линии
    for mut line in std::mem::take(&mut data.lines) {
        if data.lines.len() >= MAX_LINES {
            r.dropped_lines += 1;
            continue;
        }
        let before = line.points.len();
        line.points.retain(|p| finite2(*p));
        line.points.truncate(MAX_LINE_POINTS);
        r.dropped_points += before - line.points.len();
        if line.points.len() < 2 {
            r.dropped_lines += 1;
            continue;
        }
        for p in &mut line.points {
            let clamped = [clamp_coord(p[0]), clamp_coord(p[1])];
            if clamped != *p {
                *p = clamped;
                r.fixed_values += 1;
            }
        }
        if line.pinned.len() > 2 {
            line.pinned.truncate(2);
            r.fixed_values += 1;
        }
        data.lines.push(line);
    }

    // Скрепления
    let remap = |anchor: &SavedAnchor| -> Option<(usize, usize)> {
        let window = (*window_map.get(anchor.window)?)?;
        let piece = (*piece_maps.get(anchor.window)?.get(anchor.piece)?)?;
        Some((window, piece))
    };
    for mut joint in std::mem::take(&mut data.joints) {
        let (Some(a), Some(b)) = (remap(&joint.a), remap(&joint.b)) else {
            r.dropped_joints += 1;
            continue;
        };
        // Тело, скреплённое само с собой, физике не нужно.
        if a == b
            || !finite2(joint.a.local)
            || !finite2(joint.b.local)
            || data.joints.len() >= MAX_JOINTS
        {
            r.dropped_joints += 1;
            continue;
        }
        (joint.a.window, joint.a.piece) = a;
        (joint.b.window, joint.b.piece) = b;
        for anchor in [&mut joint.a, &mut joint.b] {
            let lim = MAX_WINDOW_SIZE as f32;
            let clamped = anchor.local.map(|v| v.clamp(-lim, lim));
            if clamped != anchor.local {
                anchor.local = clamped;
                r.fixed_values += 1;
            }
        }
        data.joints.push(joint);
    }

    r
}

/// `false` → кусок не восстановить (координаты не числа или пустой rect).
fn sanitize_piece(p: &mut SavedPiece, r: &mut ValidationReport) -> bool {
    if !finite2(p.center) || !p.rect.iter().all(|v| v.is_finite()) {
        return false;
    }
    let [x, y, w, h] = p.rect;
    if w <= 0.0 || h <= 0.0 {
        return false;
    }
    let max = MAX_WINDOW_SIZE as f64;
    let rect = [
        x.clamp(-max, max),
        y.clamp(-max, max),
        w.min(max),
        h.min(max),
    ];
    let center = [clamp_coord(p.center[0]), clamp_coord(p.center[1])];
    if rect != p.rect || center != p.center {
        p.rect = rect;
        p.center = center;
        r.fixed_values += 1;
    }
    if !p.angle.is_finite() {
        p.angle = 0.0;
        r.fixed_values += 1;
    } else if p.angle.abs() > std::f32::consts::PI {
        // Нормализация в [-π, π) — не ошибка, просто накрутившийся угол.
        p.angle = (p.angle + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
            - std::f32::consts::PI;
    }
    true
}

fn sanitize_window_props(w: &mut SavedWindow, r: &mut ValidationReport) {
    let size = w.size.map(|v| v.clamp(0, MAX_WINDOW_SIZE));
    if size != w.size {
        w.size = size;
        r.fixed_values += 1;
    }

    let weight = if w.weight.is_finite() && w.weight > 0.0 {
        w.weight.clamp(MIN_WEIGHT, MAX_WEIGHT)
    } else {
        1.0
    };
    if weight != w.weight {
        w.weight = weight;
        r.fixed_values += 1;
    }

    let softness = if w.softness.is_finite() {
        w.softness.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if softness != w.softness {
        w.softness = softness;
        r.fixed_values += 1;
    }

    if w.command.first().is_some_and(|c| c.trim().is_empty()) {
        w.command.clear();
        r.fixed_values += 1;
    }
    if w.cwd.as_deref().is_some_and(|c| c.trim().is_empty()) {
        w.cwd = None;
        r.fixed_values += 1;
    }
}

fn finite2(p: [f32; 2]) -> bool {
    p[0].is_finite() && p[1].is_finite()
}

fn clamp_coord(v: f32) -> f32 {
    v.clamp(-COORD_LIMIT, COORD_LIMIT)
}

// ── Информация о процессах ─────────────────────────────────────────

/// Процессы, которые никогда не перезапускаем сами: они не «приложения
/// пользователя», их поднимает система/сессия (по префиксу имени файла).
const HELPER_PREFIXES: &[&str] = &[
    "xdg-desktop-portal",
    "xdg-document-portal",
    "xdg-permission-store",
    "dbus-",
    "at-spi",
    "flatpak-",
    "gnome-keyring",
    "polkit",
    "ibus-",
    "fcitx",
    "pipewire",
    "wireplumber",
    "systemd",
    "Xwayland",
    "Xorg",
];

/// argv процесса для перезапуска, `None` — неизвестен или перезапускать нельзя
/// (сам композитор, Xwayland, системные помощники, подпроцессы Chromium).
/// Для Flatpak-приложений argv внутри песочницы (`/app/bin/…`) снаружи не
/// запустится, поэтому возвращаем `flatpak run <app-id>`.
pub fn process_command(pid: u32) -> Option<Vec<String>> {
    if pid == 0 || is_own_executable(pid) {
        return None;
    }
    if let Some(app) = flatpak_app_id(pid) {
        return Some(vec!["flatpak".into(), "run".into(), app]);
    }
    let argv = read_cmdline(pid)?;
    is_relaunchable(&argv).then_some(argv)
}

/// Рабочая директория процесса (абсолютный UTF-8 путь, не удалённая).
pub fn process_cwd(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let path = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let s = path.to_str()?;
    if !s.starts_with('/') || s.ends_with(" (deleted)") {
        return None;
    }
    Some(s.to_string())
}

/// Можно ли запускать такую команду при восстановлении сессии.
pub fn is_relaunchable(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    if first.trim().is_empty() {
        return false;
    }
    let name = basename(first);

    // Сам композитор: вложенный запуск восстановил бы сессию рекурсивно.
    if let Ok(own) = std::env::current_exe()
        && let Some(own) = own.file_name()
        && own.to_string_lossy().trim_end_matches(" (deleted)") == name
    {
        return false;
    }
    if name == "X" || HELPER_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    // Подпроцессы Chromium/Electron (`--type=renderer`, `--type=gpu-process`…).
    if argv[1..].iter().any(|a| a.starts_with("--type=")) {
        return false;
    }
    true
}

/// Сырой argv из `/proc/<pid>/cmdline` без фильтрации.
fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let mut argv: Vec<String> = raw
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    // cmdline заканчивается NUL → последний элемент пустой.
    if raw.last() == Some(&0) {
        argv.pop();
    }
    // Процессы, переписавшие argv через setproctitle (Chromium/Electron),
    // кладут всю командную строку одной строкой через пробелы.
    if argv.len() == 1 && argv[0].contains(' ') && !Path::new(&argv[0]).exists() {
        argv = argv[0].split_whitespace().map(String::from).collect();
    }
    if argv.first().is_none_or(|a| a.is_empty()) {
        return None;
    }
    Some(argv)
}

/// Тот же исполняемый файл, что и у нас (то есть сам композитор).
fn is_own_executable(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let (Ok(exe), Ok(own)) = (
        fs::read_link(format!("/proc/{pid}/exe")),
        std::env::current_exe(),
    ) else {
        return false;
    };
    // После пересборки бинарника ядро дописывает « (deleted)».
    let strip = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_string()
    };
    strip(&exe) == strip(&own)
}

/// app-id Flatpak-приложения из `/.flatpak-info` внутри его песочницы.
fn flatpak_app_id(pid: u32) -> Option<String> {
    let info = fs::read_to_string(format!("/proc/{pid}/root/.flatpak-info")).ok()?;
    parse_flatpak_info(&info)
}

fn parse_flatpak_info(info: &str) -> Option<String> {
    let mut in_application = false;
    for line in info.lines().map(str::trim) {
        if line.starts_with('[') {
            in_application = line == "[Application]";
            continue;
        }
        if !in_application {
            continue;
        }
        if let Some(name) = line.strip_prefix("name=").map(str::trim)
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Some(name.to_string());
        }
    }
    None
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ── Сопоставление новых окон с сохранёнными ────────────────────────

/// Порог «похожего» заголовка: общий префикс ≥ 60% длины более длинного.
const FUZZY_TITLE_MIN: f32 = 0.6;

// Очки совпадения. Порядок: app_id+заголовок > app_id+похожий заголовок >
// app_id+команда > app_id > команда > только заголовок. Команда добавляется
// поверх заголовка как уточнение.
const SCORE_APP_ID: u32 = 100;
const SCORE_TITLE_EXACT: u32 = 40;
const SCORE_TITLE_FUZZY: u32 = 20;
const SCORE_COMMAND: u32 = 10;
const SCORE_TITLE_ONLY: u32 = 5;

/// Раздаёт появляющимся окнам сохранённые места. Каждое сохранённое окно
/// достаётся максимум одному новому; через `timeout` после старта
/// невостребованные записи забываются ([`SessionRestorer::expire`]).
pub struct SessionRestorer {
    /// Ещё не занятые окна: (индекс в `SessionData::windows`, окно), в
    /// сохранённом порядке.
    pending: Vec<(usize, SavedWindow)>,
    /// `None` — не истекает никогда (переполнение `Instant`).
    deadline: Option<Instant>,
}

impl SessionRestorer {
    pub fn new(data: &SessionData, now: Instant, timeout: Duration) -> Self {
        Self {
            pending: data.windows.iter().cloned().enumerate().collect(),
            deadline: now.checked_add(timeout),
        }
    }

    /// Появилось новое окно. Возвращает лучшее незанятое сохранённое окно (и его
    /// индекс в `SessionData::windows`) и убирает его из ожидающих.
    ///
    /// Вызывать на первом коммите с буфером: Wayland-клиенты часто ставят
    /// app_id/title только к нему. X11/Wayland должны совпадать; app_id, если
    /// известен с обеих сторон, — тоже (без учёта регистра ASCII). Среди равных
    /// кандидатов берётся первый в сохранённом порядке, так что несколько окон
    /// одного приложения разбирают свои места по очереди.
    pub fn claim(
        &mut self,
        app_id: &str,
        title: &str,
        x11: bool,
        command: Option<&[String]>,
    ) -> Option<(usize, SavedWindow)> {
        let command = command.filter(|c| !c.is_empty());
        let mut best: Option<(usize, (u32, u32))> = None;
        for (pos, (_, saved)) in self.pending.iter().enumerate() {
            let Some(key) = match_key(saved, app_id, title, x11, command) else {
                continue;
            };
            // Строго больше → при равенстве остаётся более раннее окно.
            if best.is_none_or(|(_, b)| key > b) {
                best = Some((pos, key));
            }
        }
        let (pos, _) = best?;
        Some(self.pending.remove(pos))
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// После `timeout` забывает невостребованные окна (приложение не
    /// запустилось или открыло меньше окон, чем было).
    pub fn expire(&mut self, now: Instant) {
        if self.pending.is_empty() || self.deadline.is_none_or(|d| now < d) {
            return;
        }
        println!(
            "[session] {} сохранённых окон так и не появились — забываем",
            self.pending.len()
        );
        self.pending.clear();
    }
}

/// Насколько сохранённое окно подходит новому: (очки, схожесть заголовка в ‰
/// для разрешения ничьих среди «похожих»). `None` — не подходит вовсе.
fn match_key(
    saved: &SavedWindow,
    app_id: &str,
    title: &str,
    x11: bool,
    command: Option<&[String]>,
) -> Option<(u32, u32)> {
    if saved.x11 != x11 {
        return None;
    }
    let saved_app = saved.app_id.trim();
    let app_id = app_id.trim();
    let same_cmd = command.is_some_and(|c| same_command(&saved.command, c));
    let title = title.trim();
    let exact_title = !title.is_empty() && saved.title.trim() == title;

    if !saved_app.is_empty() && !app_id.is_empty() {
        if !saved_app.eq_ignore_ascii_case(app_id) {
            return None;
        }
        let mut score = SCORE_APP_ID;
        let mut tie = 0;
        if exact_title {
            score += SCORE_TITLE_EXACT;
        } else {
            let similarity = title_similarity(&saved.title, title);
            if similarity >= FUZZY_TITLE_MIN {
                score += SCORE_TITLE_FUZZY;
                tie = (similarity * 1000.0) as u32;
            }
        }
        if same_cmd {
            score += SCORE_COMMAND;
        }
        return Some((score, tie));
    }

    // app_id известен не с обеих сторон: нужен хотя бы один точный признак.
    let mut score = 0;
    if same_cmd {
        score += SCORE_COMMAND;
    }
    if exact_title {
        score += SCORE_TITLE_ONLY;
    }
    (score > 0).then_some((score, 0))
}

/// Доля общего префикса относительно более длинного заголовка (по символам):
/// `1.0` — совпадают, `0.0` — пустой или с первой буквы разные. Терминалы
/// дописывают в заголовок текущую директорию, так что `~/proj` и `~/proj/src`
/// должны считаться одним окном.
fn title_similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let longest = a.chars().count().max(b.chars().count());
    let common = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    common as f32 / longest as f32
}

/// Одна и та же команда: argv совпадает, допускается разный путь к
/// исполняемому файлу (`foot` vs `/usr/bin/foot`).
fn same_command(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return false;
    }
    a == b || (basename(&a[0]) == basename(&b[0]) && a[1..] == b[1..])
}

// ── Перезапуск приложений ──────────────────────────────────────────

/// Что запустить при старте: (argv, cwd) без повторов, в сохранённом порядке.
/// Окна одного процесса (одинаковые pid и команда) дают один запуск —
/// многооконные приложения сами восстанавливают свои окна. Окна разных
/// процессов с одинаковой командой (два отдельных терминала) запускаются
/// каждое; при неизвестном pid (0) повторы схлопываются по команде.
pub fn relaunch_commands(data: &SessionData) -> Vec<(Vec<String>, Option<String>)> {
    let mut seen: HashSet<(u32, &[String])> = HashSet::new();
    let mut out = Vec::new();
    for w in &data.windows {
        if !is_relaunchable(&w.command) || !seen.insert((w.pid, w.command.as_slice())) {
            continue;
        }
        out.push((w.command.clone(), w.cwd.clone()));
    }
    out
}

/// Запускает приложение отвязанным от композитора: своя группа процессов,
/// stdin/stdout/stderr → /dev/null, `WAYLAND_DISPLAY` наш (`DISPLAY`
/// наследуется — XWayland-окна и X11-модули Electron). Несуществующая `cwd` →
/// `$HOME`. Завершившийся процесс подбирается фоновым потоком (без зомби).
pub fn spawn(argv: &[String], cwd: Option<&str>, wayland_display: &str) -> io::Result<()> {
    let Some((program, args)) = argv.split_first().filter(|(p, _)| !p.is_empty()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "пустая команда",
        ));
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    if !wayland_display.is_empty() {
        cmd.env("WAYLAND_DISPLAY", wayland_display);
    }
    let home = env_nonempty("HOME").map(PathBuf::from);
    let dir = cwd
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .or(home.filter(|d| d.is_dir()));
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn()?;
    let _ = std::thread::Builder::new()
        .name("session-reap".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

// ── Автосохранение ─────────────────────────────────────────────────

/// Таймер периодического сохранения. Нулевой интервал — выключено.
#[derive(Debug, Clone, Copy)]
pub struct Autosave {
    pub interval: Duration,
    pub last: Instant,
}

impl Autosave {
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            last: now,
        }
    }

    /// Пора сохранять? При `true` отсчёт начинается заново от `now` (без
    /// «догоняющих» сохранений после сна/паузы).
    pub fn due(&mut self, now: Instant) -> bool {
        if self.interval.is_zero() || now.saturating_duration_since(self.last) < self.interval {
            return false;
        }
        self.last = now;
        true
    }

    /// Сохранили вне таймера (например, при выходе) — сдвигаем отсчёт.
    pub fn reset(&mut self, now: Instant) {
        self.last = now;
    }
}

// ── Тесты ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    fn piece(x: f32, y: f32) -> SavedPiece {
        SavedPiece {
            rect: [0.0, 0.0, 800.0, 600.0],
            center: [x, y],
            angle: 0.25,
            floating: false,
        }
    }

    fn window(app_id: &str, title: &str, command: &[&str], pid: u32) -> SavedWindow {
        SavedWindow {
            app_id: s(app_id),
            title: s(title),
            command: argv(command),
            pid,
            size: [800, 600],
            pieces: vec![piece(10.0, 20.0)],
            ..Default::default()
        }
    }

    fn sample() -> SessionData {
        SessionData {
            version: SESSION_VERSION,
            layout_mode: s("physics"),
            gravity_mode: s("zero"),
            camera: [-120.5, 33.0, 1.5],
            floor_y: Some(900.0),
            windows: vec![
                SavedWindow {
                    app_id: s("org.telegram.desktop"),
                    title: s("Telegram"),
                    x11: false,
                    command: argv(&["/usr/bin/telegram-desktop", "--", "-workdir"]),
                    cwd: Some(s("/home/user")),
                    pid: 4242,
                    size: [1280, 720],
                    weight: 2.5,
                    softness: 0.3,
                    pieces: vec![
                        SavedPiece {
                            rect: [0.0, 0.0, 400.0, 720.0],
                            center: [100.0, -50.25],
                            angle: -0.5,
                            floating: true,
                        },
                        SavedPiece {
                            rect: [400.0, 0.0, 880.0, 720.0],
                            center: [900.0, 10.0],
                            angle: 3.0,
                            floating: false,
                        },
                    ],
                },
                SavedWindow {
                    app_id: s("steam"),
                    title: s("Steam"),
                    x11: true,
                    command: Vec::new(),
                    cwd: None,
                    pid: 0,
                    size: [1024, 768],
                    weight: 1.0,
                    softness: 0.0,
                    pieces: vec![piece(-300.0, 0.0)],
                },
            ],
            lines: vec![
                SavedLine {
                    material: s("steel"),
                    points: vec![[0.0, 800.0], [500.0, 800.0], [1000.5, 810.0]],
                    pinned: Vec::new(),
                },
                SavedLine {
                    material: s("rope"),
                    points: vec![[10.0, 10.0], [12.0, 30.0]],
                    pinned: vec![true, false],
                },
            ],
            joints: vec![
                SavedJoint {
                    kind: s("weld"),
                    a: SavedAnchor {
                        window: 0,
                        piece: 1,
                        local: [-10.0, 5.0],
                    },
                    b: SavedAnchor {
                        window: 1,
                        piece: 0,
                        local: [0.0, 0.0],
                    },
                },
                SavedJoint {
                    kind: s("spring"),
                    a: SavedAnchor {
                        window: 0,
                        piece: 0,
                        local: [1.0, 2.0],
                    },
                    b: SavedAnchor {
                        window: 0,
                        piece: 1,
                        local: [3.0, 4.0],
                    },
                },
            ],
            workspace: 0,
            active_workspace: 0,
            workspaces: Vec::new(),
        }
    }

    /// Образец с меткой, чтобы различать версии файла.
    fn tagged(tag: &str) -> SessionData {
        let mut data = sample();
        data.windows[0].title = s(tag);
        data
    }

    /// Уникальная временная директория, удаляется при выходе из теста.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "sandboxwm-session-{name}-{}-{nanos}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn workspace_blocks_merge_with_offsets() {
        let mut top = sample();
        let mut second = sample();
        second.workspace = 2;
        second.windows.truncate(2);
        second.joints.retain(|j| j.a.window < 2 && j.b.window < 2);
        let second_joints = second.joints.len();
        top.workspaces.push(second);
        top.active_workspace = 2;

        // Вложенные столы переживают сохранение.
        let text = serialize(&top, SessionFormat::Toml).unwrap();
        let back = parse(&text, SessionFormat::Toml).unwrap();
        assert_eq!(back, top);

        let top_windows = top.windows.len();
        let top_joints = top.joints.len();
        let blocks = top.into_workspace_blocks();
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b.workspaces.is_empty()));
        let (merged, ws_of) = SessionData::merge_workspace_blocks(&blocks);
        assert_eq!(merged.windows.len(), top_windows + 2);
        assert_eq!(ws_of[0], 0);
        assert_eq!(ws_of[top_windows], 2);
        assert_eq!(merged.joints.len(), top_joints + second_joints);
        for j in &merged.joints[top_joints..] {
            assert!(j.a.window >= top_windows && j.b.window >= top_windows);
        }
    }

    #[test]
    fn toml_round_trip() {
        let data = sample();
        let text = serialize(&data, SessionFormat::Toml).unwrap();
        assert!(text.contains("[[windows.pieces]]"), "{text}");
        assert!(text.contains("[[joints]]"), "{text}");
        assert_eq!(parse(&text, SessionFormat::Toml).unwrap(), data);

        // None и пустые векторы тоже должны пережить TOML.
        let empty = SessionData::default();
        let text = serialize(&empty, SessionFormat::Toml).unwrap();
        assert!(!text.contains("floor_y"));
        assert_eq!(parse(&text, SessionFormat::Toml).unwrap(), empty);
    }

    #[test]
    fn json_round_trip() {
        let data = sample();
        let text = serialize(&data, SessionFormat::Json).unwrap();
        assert_eq!(parse(&text, SessionFormat::Json).unwrap(), data);

        let empty = SessionData::default();
        let text = serialize(&empty, SessionFormat::Json).unwrap();
        assert_eq!(parse(&text, SessionFormat::Json).unwrap(), empty);
    }

    #[test]
    fn missing_fields_take_defaults() {
        let data = parse("[[windows]]\napp_id = \"foot\"\n", SessionFormat::Toml).unwrap();
        assert_eq!(data.version, SESSION_VERSION);
        assert_eq!(data.camera, [0.0, 0.0, 1.0]);
        assert_eq!(data.windows[0].weight, 1.0);
        assert!(data.windows[0].cwd.is_none());
        assert!(parse("  \n", SessionFormat::Toml).is_err());
    }

    #[test]
    fn save_load_with_backup_fallback() {
        for format in [SessionFormat::Toml, SessionFormat::Json] {
            let tmp = TempDir::new("save");
            let path = tmp
                .0
                .join("nested")
                .join(format!("session.{}", format.extension()));
            let bak = backup_path(&path);

            // Нет файла → None без паники.
            assert!(load(&path, format).is_none());

            let first = tagged("first");
            save(&path, format, &first).unwrap();
            assert!(!bak.exists());
            assert_eq!(load(&path, format).unwrap(), first);

            let second = tagged("second");
            save(&path, format, &second).unwrap();
            assert_eq!(load(&path, format).unwrap(), second);
            assert_eq!(load(&bak, format).unwrap(), first);

            // Временные файлы не остаются.
            let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty());

            // Битый основной файл → читаем .bak.
            fs::write(&path, "{{{ это не сессия").unwrap();
            assert_eq!(load(&path, format).unwrap(), first);

            // Сохранение поверх битого файла не затирает хорошую .bak.
            let third = tagged("third");
            save(&path, format, &third).unwrap();
            assert_eq!(load(&path, format).unwrap(), third);
            assert_eq!(load(&bak, format).unwrap(), first);

            // Оба битые → None.
            fs::write(&path, "garbage = [").unwrap();
            fs::write(&bak, "").unwrap();
            assert!(load(&path, format).is_none());
        }
    }

    #[test]
    fn unknown_version_is_rejected() {
        let tmp = TempDir::new("version");
        let path = tmp.0.join("session.toml");
        fs::create_dir_all(&tmp.0).unwrap();
        fs::write(&path, "version = 99\nlayout_mode = \"tiling\"\n").unwrap();
        assert!(load(&path, SessionFormat::Toml).is_none());
        fs::write(&path, "version = 1\nlayout_mode = \"tiling\"\n").unwrap();
        assert_eq!(
            load(&path, SessionFormat::Toml).unwrap().layout_mode,
            "tiling"
        );
    }

    #[test]
    fn validation_drops_dangling_joints_and_nan() {
        let mut data = sample();
        // Окно 0 целиком негодно (NaN-центр) → индексы окон сдвигаются.
        data.windows.insert(0, {
            let mut w = window("bad", "bad", &[], 0);
            w.pieces[0].center = [f32::NAN, 0.0];
            w
        });
        // Было window 0/1 → стало 1/2. Скрепление на удалённое окно и на
        // несуществующие индексы выкидывается.
        for j in &mut data.joints {
            j.a.window += 1;
            j.b.window += 1;
        }
        data.joints.push(SavedJoint {
            kind: s("rope"),
            a: SavedAnchor {
                window: 0,
                piece: 0,
                local: [0.0, 0.0],
            },
            b: SavedAnchor {
                window: 1,
                piece: 0,
                local: [0.0, 0.0],
            },
        });
        data.joints.push(SavedJoint {
            kind: s("weld"),
            a: SavedAnchor {
                window: 1,
                piece: 7,
                local: [0.0, 0.0],
            },
            b: SavedAnchor {
                window: 9,
                piece: 0,
                local: [0.0, 0.0],
            },
        });
        data.joints.push(SavedJoint {
            kind: s("spring"),
            a: SavedAnchor {
                window: 1,
                piece: 0,
                local: [f32::INFINITY, 0.0],
            },
            b: SavedAnchor {
                window: 2,
                piece: 0,
                local: [0.0, 0.0],
            },
        });
        data.lines.push(SavedLine {
            material: s("glue"),
            points: vec![[0.0, 0.0], [f32::NAN, 1.0], [5.0, 5.0]],
            pinned: Vec::new(),
        });
        data.lines.push(SavedLine {
            material: s("trampoline"),
            points: vec![[1.0, 1.0], [f32::INFINITY, 2.0]],
            pinned: Vec::new(),
        });
        data.windows[1].size = [-5, 1_000_000];
        data.windows[1].weight = f32::NAN;
        data.camera = [f64::NAN, 1.0, -2.0];
        data.floor_y = Some(f32::INFINITY);

        let report = validate(&mut data);
        assert_eq!(report.dropped_windows, 1);
        assert_eq!(report.dropped_pieces, 1);
        assert_eq!(report.dropped_joints, 3);
        assert_eq!(report.dropped_lines, 1);
        assert_eq!(report.dropped_points, 2);
        assert!(report.fixed_values >= 4);

        assert_eq!(data.windows.len(), 2);
        assert_eq!(data.windows[0].app_id, "org.telegram.desktop");
        assert_eq!(data.windows[0].size, [0, MAX_WINDOW_SIZE]);
        assert_eq!(data.windows[0].weight, 1.0);
        assert_eq!(data.camera, [0.0, 1.0, 1.0]);
        assert_eq!(data.floor_y, None);

        // Исходные скрепления пережили перенумерацию.
        let original = sample().joints;
        assert_eq!(data.joints, original);
        assert_eq!(data.lines.len(), 3);
        assert_eq!(data.lines[2].points, vec![[0.0, 0.0], [5.0, 5.0]]);

        // Повторная валидация ничего не меняет.
        assert!(validate(&mut data).is_clean());
    }

    #[test]
    fn save_sanitizes_nan_for_json() {
        let tmp = TempDir::new("nan");
        let path = tmp.0.join("session.json");
        let mut data = sample();
        data.windows[0].pieces[0].angle = f32::NAN;
        data.lines[0].points.push([f32::NAN, f32::NAN]);
        save(&path, SessionFormat::Json, &data).unwrap();
        let loaded = load(&path, SessionFormat::Json).unwrap();
        assert_eq!(loaded.windows[0].pieces[0].angle, 0.0);
        assert_eq!(loaded.lines[0].points.len(), 3);
    }

    #[test]
    fn matcher_priority() {
        let data = SessionData {
            windows: vec![
                window("foot", "x", &["foot"], 1),
                window("foot", "y", &["foot", "-a"], 2),
                window("foot", "z", &["foot", "--server"], 3),
                window("org.telegram.desktop", "Telegram", &["telegram-desktop"], 4),
                SavedWindow {
                    x11: true,
                    ..window("steam", "Steam", &["steam"], 5)
                },
            ],
            ..Default::default()
        };
        let now = Instant::now();
        let mut r = SessionRestorer::new(&data, now, Duration::from_secs(30));
        let foot_a = argv(&["foot", "-a"]);
        let foot_server = argv(&["/usr/bin/foot", "--server"]);

        // Точный заголовок важнее команды.
        let (i, w) = r.claim("foot", "x", false, Some(&foot_a)).unwrap();
        assert_eq!((i, w.title.as_str()), (0, "x"));
        // Команда (в т.ч. с другим путём к бинарнику) важнее порядка.
        assert_eq!(r.claim("foot", "", false, Some(&foot_server)).unwrap().0, 2);
        // Просто app_id.
        assert_eq!(r.claim("foot", "other", false, None).unwrap().0, 1);
        assert!(r.claim("foot", "x", false, None).is_none());

        // X11 и Wayland не путаются.
        assert!(r.claim("steam", "Steam", false, None).is_none());
        // Разные непустые app_id не совпадают даже при одинаковой команде.
        let tg = argv(&["/usr/bin/telegram-desktop"]);
        assert!(r.claim("org.other", "Telegram", false, Some(&tg)).is_none());
        // app_id ещё не выставлен — узнаём по команде.
        assert_eq!(r.claim("", "", false, Some(&tg)).unwrap().0, 3);
        assert_eq!(r.claim("Steam", "", true, None).unwrap().0, 4);
        assert!(!r.is_pending());
    }

    #[test]
    fn matcher_multi_window_order() {
        let data = SessionData {
            windows: vec![
                window("firefox", "GitHub — Mozilla Firefox", &["firefox"], 7),
                window("foot", "shell", &["foot"], 8),
                window("firefox", "YouTube — Mozilla Firefox", &["firefox"], 7),
                window("firefox", "Docs — Mozilla Firefox", &["firefox"], 7),
            ],
            ..Default::default()
        };
        let mut r = SessionRestorer::new(&data, Instant::now(), Duration::from_secs(30));
        let order: Vec<usize> = (0..3)
            .filter_map(|_| r.claim("firefox", "", false, None).map(|(i, _)| i))
            .collect();
        assert_eq!(order, vec![0, 2, 3]);
        assert!(r.claim("firefox", "", false, None).is_none());
        assert_eq!(r.pending_count(), 1);
    }

    #[test]
    fn matcher_fuzzy_title() {
        assert_eq!(title_similarity("abc", "abc"), 1.0);
        assert_eq!(title_similarity("", "abc"), 0.0);
        assert!(title_similarity("user@host: ~/proj", "user@host: ~/proj/src") >= FUZZY_TITLE_MIN);
        assert!(title_similarity("GitHub — Firefox", "Gmail — Firefox") < FUZZY_TITLE_MIN);

        let data = SessionData {
            windows: vec![
                window("foot", "user@host: ~/music", &["foot"], 1),
                window("foot", "user@host: ~/projects/sandboxWM", &["foot"], 2),
                window("foot", "user@host: ~/projects/sandboxWM/src", &["foot"], 3),
            ],
            ..Default::default()
        };
        let mut r = SessionRestorer::new(&data, Instant::now(), Duration::from_secs(30));
        // Ближе всего к окну 2 (длиннее общий префикс), хотя 1 раньше по порядку.
        let (i, _) = r
            .claim(
                "foot",
                "user@host: ~/projects/sandboxWM/src/main.rs",
                false,
                None,
            )
            .unwrap();
        assert_eq!(i, 2);
        let (i, _) = r
            .claim(
                "foot",
                "user@host: ~/projects/sandboxWM/target",
                false,
                None,
            )
            .unwrap();
        assert_eq!(i, 1);
        assert_eq!(r.claim("foot", "htop", false, None).unwrap().0, 0);
    }

    #[test]
    fn restorer_expires() {
        let data = SessionData {
            windows: vec![window("foot", "", &["foot"], 1)],
            ..Default::default()
        };
        let now = Instant::now();
        let mut r = SessionRestorer::new(&data, now, Duration::from_secs(10));
        r.expire(now + Duration::from_secs(5));
        assert!(r.is_pending());
        r.expire(now + Duration::from_secs(10));
        assert!(!r.is_pending());
        assert!(r.claim("foot", "", false, None).is_none());

        // Бесконечный таймаут не переполняется и не истекает.
        let mut r = SessionRestorer::new(&data, now, Duration::MAX);
        r.expire(now + Duration::from_secs(1_000_000));
        assert!(r.is_pending());
    }

    #[test]
    fn relaunch_dedupe() {
        let mut data = SessionData {
            windows: vec![
                window("firefox", "a", &["firefox"], 10),
                window("firefox", "b", &["firefox"], 10),
                window("foot", "1", &["foot"], 20),
                window("foot", "2", &["foot"], 21),
                window("foot", "3", &["foot"], 0),
                window("foot", "4", &["foot"], 0),
                window("x", "", &[], 30),
                window("portal", "", &["/usr/libexec/xdg-desktop-portal-gtk"], 31),
                window("xw", "", &["/usr/bin/Xwayland", ":0"], 32),
                window("chromium", "", &["chromium", "--type=renderer"], 33),
            ],
            ..Default::default()
        };
        data.windows[2].cwd = Some(s("/tmp"));
        let cmds = relaunch_commands(&data);
        let expected = vec![
            (argv(&["firefox"]), None),
            (argv(&["foot"]), Some(s("/tmp"))),
            (argv(&["foot"]), None),
            (argv(&["foot"]), None),
        ];
        assert_eq!(cmds, expected);
    }

    #[test]
    fn process_info_for_current_process() {
        let pid = std::process::id();
        let expected: Vec<String> = std::env::args().collect();
        assert_eq!(read_cmdline(pid).unwrap(), expected);
        // Тестовый бинарник = current_exe → считается «самим композитором».
        assert!(is_own_executable(pid));
        assert!(process_command(pid).is_none());
        assert!(!is_relaunchable(&expected));

        let cwd = std::env::current_dir().unwrap();
        assert_eq!(process_cwd(pid).unwrap(), cwd.to_string_lossy());

        assert!(process_command(0).is_none());
        assert!(process_command(u32::MAX).is_none());
        assert!(process_cwd(u32::MAX).is_none());

        assert!(is_relaunchable(&argv(&["foot"])));
        assert!(is_relaunchable(&argv(&[
            "flatpak",
            "run",
            "org.mozilla.firefox"
        ])));
        assert!(!is_relaunchable(&[]));
        assert!(!is_relaunchable(&argv(&["", "x"])));
        assert!(!is_relaunchable(&argv(&["dbus-daemon", "--session"])));
    }

    #[test]
    fn flatpak_info_parsing() {
        let info = "[Instance]\nname=nope\n\n[Application]\nname=org.mozilla.firefox\nruntime=x\n";
        assert_eq!(
            parse_flatpak_info(info).as_deref(),
            Some("org.mozilla.firefox")
        );
        assert_eq!(parse_flatpak_info("[Application]\nname=bad id;rm\n"), None);
        assert_eq!(parse_flatpak_info(""), None);
    }

    #[test]
    fn path_resolution() {
        let toml = SessionFormat::Toml;
        let json = SessionFormat::Json;
        assert_eq!(
            resolve_session_path("~/s/my.toml", toml, Some("/state"), Some("/h")),
            PathBuf::from("/h/s/my.toml")
        );
        assert_eq!(
            resolve_session_path("/abs/dir/", json, None, Some("/h")),
            PathBuf::from("/abs/dir/session.json")
        );
        assert_eq!(
            resolve_session_path("  ", json, Some("/state"), Some("/h")),
            PathBuf::from("/state/sandboxWM/session.json")
        );
        assert_eq!(
            resolve_session_path("", toml, Some("relative"), Some("/h")),
            PathBuf::from("/h/.local/state/sandboxWM/session.toml")
        );
        assert_eq!(
            resolve_session_path("", toml, None, None),
            std::env::temp_dir().join("sandboxWM").join("session.toml")
        );
        assert_eq!(
            backup_path(Path::new("/a/session.toml")),
            PathBuf::from("/a/session.toml.bak")
        );
    }

    #[test]
    fn format_from_config() {
        assert_eq!(SessionFormat::from_config("json"), SessionFormat::Json);
        assert_eq!(SessionFormat::from_config(" JSON "), SessionFormat::Json);
        assert_eq!(SessionFormat::from_config("toml"), SessionFormat::Toml);
        assert_eq!(SessionFormat::from_config("yaml"), SessionFormat::Toml);
    }

    #[test]
    fn autosave_timer() {
        let now = Instant::now();
        let mut a = Autosave::new(Duration::from_secs(60), now);
        assert!(!a.due(now + Duration::from_secs(59)));
        assert!(a.due(now + Duration::from_secs(60)));
        assert!(!a.due(now + Duration::from_secs(61)));
        assert!(a.due(now + Duration::from_secs(200)));

        let mut off = Autosave::new(Duration::ZERO, now);
        assert!(!off.due(now + Duration::from_secs(3600)));
    }

    #[test]
    fn spawn_basic() {
        assert!(spawn(&[], None, "wayland-test").is_err());
        assert!(spawn(&argv(&[""]), None, "wayland-test").is_err());
        assert!(spawn(&argv(&["/nonexistent/sandboxwm-binary"]), None, "w").is_err());
        assert!(spawn(&argv(&["true"]), Some("/nonexistent-dir"), "wayland-test").is_ok());
    }
}
