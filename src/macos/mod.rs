//! Модуль macOS: реализация доступа к системе (активное окно, раскладка),
//! меню-бар UI и режим печати (event tap).

pub mod event_tap;
pub mod ffi;
pub mod headless;
pub mod keymap;
pub mod system;
pub mod ui;

pub use system::MacSystem;
