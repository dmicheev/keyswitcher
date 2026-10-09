//! Модуль macOS: реализация доступа к системе (активное окно, раскладка).

pub mod ffi;
pub mod system;

pub use system::MacSystem;
