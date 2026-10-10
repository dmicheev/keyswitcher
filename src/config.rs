//! Конфигурация SmartSwitcher.
//!
//! Формат файла — простой `ключ = значение`, строки начинающиеся с `#` — комментарии.
//! Файл лежит рядом с исполняемым файлом (портативный режим):
//! `<каталог программы>/config.txt`, например `target/release/config.txt`.
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
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Каталог, в котором лежит сам исполняемый файл: конфиг и журнал
/// пишутся рядом с программой (портативный режим). Если путь определить
/// не удалось — текущий каталог процесса.
pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

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
    /// Путь к файлу конфигурации по умолчанию — `config.txt` рядом с программой.
    pub fn default_path() -> PathBuf {
        base_dir().join("config.txt")
    }

    /// Загружает конфигурацию из файла. Если файла нет — возвращает Default.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => Self::parse(&content),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                log::info!(
                    "Конфигурация не найдена в {:?}, используются значения по умолчанию",
                    path
                );
                Self::default()
            }
            Err(e) => {
                log::warn!(
                    "Не удалось прочитать конфигурацию {:?}: {} — используются значения по умолчанию",
                    path,
                    e
                );
                Self::default()
            }
        }
    }

    /// Сериализует конфигурацию в формат файла (для окна настроек).
    pub fn to_file_string(&self) -> String {
        let default = match self.default_layout {
            DefaultLayout::En => "en",
            DefaultLayout::Ru => "ru",
            DefaultLayout::Keep => "keep",
        };
        // HashSet неупорядочен — сортируем, чтобы файл не «дрожал» при перезаписи.
        let mut excluded: Vec<String> = self.excluded_apps.iter().cloned().collect();
        excluded.sort();
        format!(
            "# SmartSwitcher — конфигурация (рядом с бинарником)\n\
             # Перечитывается на лету раз в 5 секунд, перезапуск не нужен.\n\n\
             # Автоматическая коррекция раскладки под активное окно\n\
             auto_correct = {}\n\n\
             # Раскладка по умолчанию для новых окон: en | ru | keep\n\
             default_layout = {}\n\n\
             # Секунды, после которых сохранённая раскладка окна считается неактуальной\n\
             idle_pause_seconds = {}\n\n\
             # Приложения, для которых коррекция НЕ применяется (через запятую)\n\
             exclude_apps = {}\n",
            self.auto_correct,
            default,
            self.idle_pause_seconds,
            excluded.join(",")
        )
    }

    /// Сохраняет конфигурацию в файл.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_file_string())
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
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        other => {
            log::warn!(
                "Неизвестное логическое значение '{}', используется false",
                other
            );
            false
        }
    }
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

    #[test]
    fn test_parse_bad_values_fall_back_to_defaults() {
        let cfg = Config::parse(
            "\
auto_correct =banana
default_layout = de
idle_pause_seconds = -5
",
        );
        // auto_correct: неизвестное значение -> false (с предупреждением в лог).
        assert!(!cfg.auto_correct);
        // default_layout: неизвестная раскладка -> Keep.
        assert_eq!(cfg.default_layout, DefaultLayout::Keep);
        // idle_pause_seconds: не число -> дефолт.
        assert_eq!(cfg.idle_pause_seconds, Config::default().idle_pause_seconds);
    }

    #[test]
    fn test_paths_are_portable_next_to_executable() {
        // Конфиг и база для журнала лежат в одном каталоге — рядом с бинарём.
        let base = base_dir();
        assert_eq!(Config::default_path(), base.join("config.txt"));
        assert!(Config::default_path().is_absolute());
    }

    #[test]
    fn test_save_roundtrip() {
        let original = Config {
            auto_correct: true,
            default_layout: DefaultLayout::Ru,
            idle_pause_seconds: 300,
            excluded_apps: ["Terminal".into(), "1Password".into()]
                .into_iter()
                .collect(),
        };
        let reloaded = Config::parse(&original.to_file_string());
        assert_eq!(reloaded.auto_correct, original.auto_correct);
        assert_eq!(reloaded.default_layout, original.default_layout);
        assert_eq!(reloaded.idle_pause_seconds, original.idle_pause_seconds);
        assert_eq!(reloaded.excluded_apps, original.excluded_apps);
    }
}
