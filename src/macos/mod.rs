//! Модуль macOS: реализация доступа к системе (активное окно, раскладка)
//! и меню-бар UI.

pub mod ffi;
pub mod system;
pub mod ui;

pub use system::MacSystem;
