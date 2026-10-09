//! Конфигурация PuntoSwitcher.
//!
//! Формат файла — простой `ключ = значение`, строки начинающиеся с `#` — комментарии.
//! Пример (`~/Library/Application Support/puntoswicher/config.txt`):
//!
//! ```text
//! # Включить автоматическую коррекцию раскладки (EN <-> RU)
//! auto_correct = true
//!
//! # Раскладка по умолчанию для новых окон: en | ru | keep
//! default_layout = keep
//!
//! # Пауза в секундах после которой состояние считается неактуальным
//! idle_pause_seconds = 60
//!
//! # Список приложений, для которых коррекция НЕ применяется (через запятую)
//! exclude_apps = 1Password,KeePassXC
//! ```

use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultLayout {
    En,
    Ru,
    /// Оставлять раскладку такой, какой она была у окна ранее / как есть.
    Keep,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Разрешена ли автоматическая коррекция раскладки под активным окном.
    pub auto_correct: bool,
    /// Раскладка, применяемая к новым окнам/приложениям.
    pub default_layout: DefaultLayout,
    /// Если пользователь не переключал окно дольше этого значения секунд —
    /// считать сохранённое состояние раскладки устаревшим.
    pub idle_pause_seconds: u64,
    /// Приложения (по имени процесса), для которых коррекция отключена.
    pub excluded_apps: HashSet<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_correct: true,
            default_layout: DefaultLayout::Keep,
            idle_pause_seconds: 60,
            excluded_apps: HashSet::new(),
        }
    }
}

impl Config {
    /// Путь к файлу конфигурации по умолчанию.
    pub fn default_path() -> PathBuf {
        // На macOS: ~/Library/Application Support/puntoswicher/config.txt
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("puntoswicher")
            .join("config.txt")
    }

    /// Загружает конфигурацию из файла. Если файла нет — возвращает Default.
    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => Self::parse(&content),
            Err(_) => {
                log::info!(
                    "Конфигурация не найдена в {:?}, используются значения по умолчанию",
                    path
                );
                Self::default()
            }
        }
    }

    pub fn parse(content: &str) -> Self {
        let mut cfg = Self::default();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            match key {
                "auto_correct" => cfg.auto_correct = parse_bool(value),
                "default_layout" => {
                    cfg.default_layout = match value.to_ascii_lowercase().as_str() {
                        "en" => DefaultLayout::En,
                        "ru" => DefaultLayout::Ru,
                        _ => DefaultLayout::Keep,
                    };
                }
                "idle_pause_seconds" => {
                    cfg.idle_pause_seconds = value.parse().unwrap_or(cfg.idle_pause_seconds)
                }
                "exclude_apps" => {
                    cfg.excluded_apps = value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                other => log::warn!("Неизвестный параметр конфигурации: '{}'", other),
            }
        }
        cfg
    }

    pub fn is_excluded(&self, app_name: &str) -> bool {
        self.excluded_apps
            .iter()
            .any(|e| e.eq_ignore_ascii_case(app_name))
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let cfg = Config::default();
        assert!(cfg.auto_correct);
        assert_eq!(cfg.default_layout, DefaultLayout::Keep);
    }

    #[test]
    fn test_parse() {
        let cfg = Config::parse(
            "
# комментарий
auto_correct = false
default_layout = ru
idle_pause_seconds = 30
exclude_apps = 1Password, KeePassXC
",
        );
        assert!(!cfg.auto_correct);
        assert_eq!(cfg.default_layout, DefaultLayout::Ru);
        assert_eq!(cfg.idle_pause_seconds, 30);
        assert!(cfg.is_excluded("1password"));
        assert!(cfg.is_excluded("KeePassXC"));
        assert!(!cfg.is_excluded("Safari"));
    }
}
