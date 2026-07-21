English version
sandboxWM — экспериментальный Wayland-композитор на Rust: обычный BSP/Dwindle-тайлинг можно переключить в физическую песочницу, где окна имеют массу, падают, сталкиваются и вращаются.
Нужны Rust, системные библиотеки Wayland/DRM/GBM/EGL и libseat для нативного режима.
Режим выбирается автоматически: если нет WAYLAND_DISPLAY и DISPLAY, запускается DRM/KMS; иначе — Winit.
При старте читается первая найденная директория:
Все файлы *.conf в этой директории объединяются как TOML. Сейчас реально применяются [physics] и [controls] из config/sandboxWM.conf. Остальные секции (drawing, materials, joints, soft_body, slicing, session, window_props) уже имеют схему и примеры конфигурации, но пока не подключены к логике.
Подробное целевое видение — в sandboxWM_concept.md: физические линии из стали/клея/батута/верёвки, сохранение сессии, Weld/Spring/Rope joints, мягкие окна, обратное проецирование ввода и слайсинг через wp_viewporter.
Rust 2024 · Smithay 0.7 · rapier2d 0.34 · GLES/EGL · GBM/DRM · libinput · serde/TOML