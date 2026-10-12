//! Конфигурация SmartSwitcher.
//!
//! Формат файла — простой `ключ = значение`, строки начинающиеся с `#` — комментарии.
//! Файл лежит в одном месте: `~/.config/smartswitcher/config.txt`
//! (или `$XDG_CONFIG_HOME/smartswitcher/config.txt`, если переменная задана).
//! Создаётся автоматически при первом запуске; конфиг из старого
//! «портативного» расположения (рядом с бинарником) переносится один раз.
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
//!
//! # Автопереключение раскладки по ходу печати (Punto-стиль)
//! typing_detect = true
//!
//! # Минимальная длина префикса слова для начала скоринга
//! min_prefix_chars = 3
//!
//! # Порог уверенности для автопереключения (во «сколько раз»)
//! switch_threshold = 100.0
//!
//! # Исправлять уже набранный «неправильный» префикс слова
//! retro_fix = true
//!
//! # Горячая клавиша конвертации последнего слова: [mods+]key
//! convert_hotkey = f13
//!
//! # Спеллчек-подсказки: попап с вариантами исправления по завершении слова
//! spell_suggest = true
//!
//! # Минимальная длина слова для спеллчек-подсказок
//! spell_min_chars = 4
//!
//! # Сколько вариантов исправления показывать (1..9)
//! spell_max_suggestions = 5
//! ```

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::keymap;

/// Каталог конфигурации: одно место для всех данных программы —
/// `$XDG_CONFIG_HOME/smartswitcher`, иначе `~/.config/smartswitcher`.
/// Рядом с `config.txt` лежит и журнал (`smartswitcher.log`).
pub fn config_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("smartswitcher");
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("smartswitcher")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultLayout {
    En,
    Ru,
    /// Оставлять раскладку такой, какой она была у окна ранее / как есть.
    Keep,
}

/// Модификаторы горячей клавиши конвертации.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HotkeyMods {
    pub ctrl: bool,
    pub shift: bool,
    pub option: bool,
    pub cmd: bool,
}

/// Горячая клавиша конвертации последнего слова: модификаторы + код клавиши.
/// `key: None` — хоткей «тап модификатора»: срабатывает на отпускании
/// заданной комбинации модификаторов, если между нажатием и отпусканием
/// не было других клавиш (например, `option`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: HotkeyMods,
    /// Виртуальный код клавиши macOS (например, F13 = 0x69);
    /// `None` — только модификаторы.
    pub key: Option<u8>,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            mods: HotkeyMods {
                ctrl: false,
                shift: false,
                option: true,
                cmd: false,
            },
            key: None,
        }
    }
}

impl Hotkey {
    /// Разбирает строку вида `option`, `ctrl+shift+f13`, `cmd+t`.
    /// Поддерживаются f1–f19, буквы a–z и комбинации модификаторов без
    /// клавиши (`option`, `ctrl+shift`, ...). Пустая строка — ошибка.
    pub fn parse(s: &str) -> Option<Self> {
        let mut mods = HotkeyMods::default();
        let mut key = None;
        let mut any_mod = false;
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    mods.ctrl = true;
                    any_mod = true;
                }
                "shift" => {
                    mods.shift = true;
                    any_mod = true;
                }
                "alt" | "opt" | "option" => {
                    mods.option = true;
                    any_mod = true;
                }
                "cmd" | "command" => {
                    mods.cmd = true;
                    any_mod = true;
                }
                name => {
                    if key.is_some() {
                        return None; // две «клавиши» в строке
                    }
                    key = Some(parse_key(name)?);
                }
            }
        }
        (any_mod || key.is_some()).then_some(Self { mods, key })
    }

    /// Хоткей без клавиши — «тап модификаторов».
    pub fn is_mod_only(&self) -> bool {
        self.key.is_none()
    }

    /// Каноничное представление для config.txt.
    pub fn to_display_string(self) -> String {
        let HotkeyMods {
            ctrl,
            shift,
            option,
            cmd,
        } = self.mods;
        let mut parts: Vec<&str> = Vec::new();
        if ctrl {
            parts.push("ctrl");
        }
        if shift {
            parts.push("shift");
        }
        if option {
            parts.push("option");
        }
        if cmd {
            parts.push("cmd");
        }
        let mut out = parts.join("+");
        if let Some(key) = self.key {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(&key_name(key));
        }
        out
    }
}

/// Клавиша по имени: f1–f19 или буква a–z.
fn parse_key(name: &str) -> Option<u8> {
    // F-клавиши в порядке f1..f19.
    const F_KEYS: [u8; 19] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71,
        0x6A, 0x40, 0x4F, 0x50,
    ];
    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(n) = rest.parse::<usize>() {
            return (1..=19).contains(&n).then(|| F_KEYS[n - 1]);
        }
        return None;
    }
    if name.len() != 1 {
        return None;
    }
    let c = name.chars().next()?;
    keymap::letter_keycode(c.to_ascii_lowercase())
}

/// Имя клавиши по коду (обратно к [`parse_key`]).
fn key_name(code: u8) -> String {
    const F_NAMES: [(u8, &str); 19] = [
        (0x7A, "f1"),
        (0x78, "f2"),
        (0x63, "f3"),
        (0x76, "f4"),
        (0x60, "f5"),
        (0x61, "f6"),
        (0x62, "f7"),
        (0x64, "f8"),
        (0x65, "f9"),
        (0x6D, "f10"),
        (0x67, "f11"),
        (0x6F, "f12"),
        (0x69, "f13"),
        (0x6B, "f14"),
        (0x71, "f15"),
        (0x6A, "f16"),
        (0x40, "f17"),
        (0x4F, "f18"),
        (0x50, "f19"),
    ];
    for (c, name) in F_NAMES {
        if c == code {
            return name.to_string();
        }
    }
    keymap::letter_name(code).map_or_else(|| format!("0x{code:02x}"), |c| c.to_string())
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
    /// Автопереключение раскладки по ходу печати (Punto-стиль).
    pub typing_detect: bool,
    /// Минимальная длина префикса слова для начала скоринга.
    pub min_prefix_chars: usize,
    /// Порог уверенности для автопереключения («во сколько раз»).
    pub switch_threshold: f64,
    /// Исправлять уже набранный префикс слова (backspace × k + ввод).
    pub retro_fix: bool,
    /// Горячая клавиша конвертации последнего слова.
    pub convert_hotkey: Hotkey,
    /// Спеллчек-подсказки: попап с вариантами исправления по завершении слова.
    pub spell_suggest: bool,
    /// Минимальная длина слова для спеллчек-подсказок.
    pub spell_min_chars: usize,
    /// Сколько вариантов исправления показывать (1..9).
    pub spell_max_suggestions: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_correct: true,
            default_layout: DefaultLayout::Keep,
            idle_pause_seconds: 60,
            excluded_apps: HashSet::new(),
            typing_detect: true,
            min_prefix_chars: 3,
            switch_threshold: 100.0,
            retro_fix: true,
            convert_hotkey: Hotkey::default(),
            spell_suggest: true,
            spell_min_chars: 4,
            spell_max_suggestions: 5,
        }
    }
}

impl Config {
    /// Путь к файлу конфигурации — `~/.config/smartswitcher/config.txt`.
    pub fn default_path() -> PathBuf {
        config_dir().join("config.txt")
    }

    /// Одноразовая миграция из «портативного» расположения (config.txt рядом
    /// с бинарником): копирует старый файл, только если нового ещё нет.
    /// Возвращает путь к перенесённому файлу (для журнала).
    pub fn migrate_portable_config() -> Option<PathBuf> {
        let new_path = Self::default_path();
        if new_path.exists() {
            return None;
        }
        let old = std::env::current_exe().ok()?.parent()?.join("config.txt");
        if !old.exists() {
            return None;
        }
        if let Some(dir) = new_path.parent() {
            std::fs::create_dir_all(dir).ok()?;
        }
        match std::fs::copy(&old, &new_path) {
            Ok(_) => Some(old),
            Err(e) => {
                log::warn!("Не удалось перенести конфиг {old:?}: {e}");
                None
            }
        }
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
            "# SmartSwitcher — конфигурация ({})\n\
             # Перечитывается на лету раз в 5 секунд, перезапуск не нужен.\n\n\
             # Автоматическая коррекция раскладки под активное окно\n\
             auto_correct = {}\n\n\
             # Раскладка по умолчанию для новых окон: en | ru | keep\n\
             default_layout = {}\n\n\
             # Секунды, после которых сохранённая раскладка окна считается неактуальной\n\
             idle_pause_seconds = {}\n\n\
             # Приложения, для которых коррекция НЕ применяется (через запятую)\n\
             exclude_apps = {}\n\n\
             # Автопереключение раскладки по ходу печати (Punto-стиль)\n\
             typing_detect = {}\n\n\
             # Минимальная длина префикса слова для начала скоринга\n\
             min_prefix_chars = {}\n\n\
             # Порог уверенности для автопереключения («во сколько раз»)\n\
             switch_threshold = {}\n\n\
             # Исправлять уже набранный «неправильный» префикс слова\n\
             retro_fix = {}\n\n\
             # Горячая клавиша конвертации последнего слова (например ctrl+shift+f13)\n\
             convert_hotkey = {}\n\n\
             # Спеллчек-подсказки: после разделителя показывать попап с вариантами\n\
             # исправления, если слово похоже на опечатку (клик или цифры 1-9)\n\
             spell_suggest = {}\n\n\
             # Минимальная длина слова для спеллчек-подсказок\n\
             spell_min_chars = {}\n\n\
             # Сколько вариантов исправления показывать (1..9)\n\
             spell_max_suggestions = {}\n",
            config_dir().display(),
            self.auto_correct,
            default,
            self.idle_pause_seconds,
            excluded.join(","),
            self.typing_detect,
            self.min_prefix_chars,
            self.switch_threshold,
            self.retro_fix,
            self.convert_hotkey.to_display_string(),
            self.spell_suggest,
            self.spell_min_chars,
            self.spell_max_suggestions,
        )
    }

    /// Сохраняет конфигурацию в файл (каталог создаётся при необходимости).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
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
                "auto_correct" => {
                    if let Some(b) = parse_bool(value) {
                        cfg.auto_correct = b;
                    }
                }
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
                "typing_detect" => {
                    if let Some(b) = parse_bool(value) {
                        cfg.typing_detect = b;
                    }
                }
                "min_prefix_chars" => {
                    cfg.min_prefix_chars = value.parse().unwrap_or(cfg.min_prefix_chars)
                }
                "switch_threshold" => {
                    cfg.switch_threshold = value.parse().unwrap_or(cfg.switch_threshold)
                }
                "retro_fix" => {
                    if let Some(b) = parse_bool(value) {
                        cfg.retro_fix = b;
                    }
                }
                "convert_hotkey" => match Hotkey::parse(value) {
                    Some(h) => cfg.convert_hotkey = h,
                    None => log::warn!("Некорректный convert_hotkey '{}', оставлен прежний", value),
                },
                "spell_suggest" => {
                    if let Some(b) = parse_bool(value) {
                        cfg.spell_suggest = b;
                    }
                }
                "spell_min_chars" => {
                    cfg.spell_min_chars = value.parse().unwrap_or(cfg.spell_min_chars)
                }
                "spell_max_suggestions" => {
                    cfg.spell_max_suggestions = value.parse().unwrap_or(cfg.spell_max_suggestions)
                }
                other => log::warn!("Неизвестный параметр конфигурации: '{}'", other),
            }
        }
        cfg
    }

    /// Сравнение без учёта регистра (полный Unicode: имена приложений
    /// могут быть кириллическими, `eq_ignore_ascii_case` их не сводит).
    pub fn is_excluded(&self, app_name: &str) -> bool {
        let target = app_name.to_lowercase();
        self.excluded_apps
            .iter()
            .any(|e| e.to_lowercase() == target)
    }
}

/// Булево значение файла конфигурации. `None` — нераспознанное значение:
/// как и для чисел, сохраняется прежнее (не выключаем фичу из-за опечатки).
fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            log::warn!("Неизвестное логическое значение '{other}', сохранено прежнее");
            None
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
        // Режим печати включён по умолчанию.
        assert!(cfg.typing_detect);
        assert_eq!(cfg.min_prefix_chars, 3);
        assert_eq!(cfg.switch_threshold, 100.0);
        assert!(cfg.retro_fix);
        // Хоткей по умолчанию — тап Option (без клавиши).
        assert_eq!(cfg.convert_hotkey, Hotkey::default());
        assert!(cfg.convert_hotkey.mods.option);
        assert!(cfg.convert_hotkey.is_mod_only());
        // Спеллчек-подсказки включены по умолчанию.
        assert!(cfg.spell_suggest);
        assert_eq!(cfg.spell_min_chars, 4);
        assert_eq!(cfg.spell_max_suggestions, 5);
    }

    #[test]
    fn test_parse_spell_keys() {
        let cfg = Config::parse(
            "
spell_suggest = false
spell_min_chars = 6
spell_max_suggestions = 3
",
        );
        assert!(!cfg.spell_suggest);
        assert_eq!(cfg.spell_min_chars, 6);
        assert_eq!(cfg.spell_max_suggestions, 3);
    }

    #[test]
    fn test_parse_typing_keys() {
        let cfg = Config::parse(
            "
typing_detect = true
min_prefix_chars = 4
switch_threshold = 250.5
retro_fix = false
convert_hotkey = ctrl+shift+f13
",
        );
        assert!(cfg.typing_detect);
        assert_eq!(cfg.min_prefix_chars, 4);
        assert_eq!(cfg.switch_threshold, 250.5);
        assert!(!cfg.retro_fix);
        let hk = Hotkey::parse("ctrl+shift+f13").expect("hotkey");
        assert_eq!(cfg.convert_hotkey, hk);
        assert!(hk.mods.ctrl && hk.mods.shift && !hk.mods.option && !hk.mods.cmd);
        assert_eq!(hk.key, Some(crate::keymap::codes::F13));
    }

    #[test]
    fn test_hotkey_parse_and_display() {
        assert_eq!(Hotkey::parse("f13").and_then(|h| h.key), Some(0x69));
        assert_eq!(Hotkey::parse("F1").and_then(|h| h.key), Some(0x7A));
        assert_eq!(Hotkey::parse("cmd+t").and_then(|h| h.key), Some(0x11)); // t
                                                                            // alt и opt — один и тот же модификатор; две клавиши в строке — ошибка.
        assert!(Hotkey::parse("alt+opt+f5").is_some());
        assert_eq!(Hotkey::parse("f5+f7"), None);
        assert_eq!(Hotkey::parse("nonsense"), None);
        assert_eq!(Hotkey::parse(""), None);
        // Хоткей «только модификаторы» (тап).
        let tap = Hotkey::parse("option").expect("option");
        assert!(tap.is_mod_only());
        assert!(tap.mods.option && !tap.mods.ctrl && !tap.mods.shift && !tap.mods.cmd);
        assert_eq!(tap.to_display_string(), "option");
        assert_eq!(Hotkey::parse("alt").unwrap().to_display_string(), "option");
        assert_eq!(
            Hotkey::parse("ctrl+shift").unwrap().to_display_string(),
            "ctrl+shift"
        );
        // Классические варианты.
        assert_eq!(
            Hotkey::parse("ctrl+shift+f13").unwrap().to_display_string(),
            "ctrl+shift+f13"
        );
        assert_eq!(Hotkey::parse("f7").unwrap().to_display_string(), "f7");
        assert_eq!(Hotkey::parse("cmd+z").unwrap().to_display_string(), "cmd+z");
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
        // auto_correct: неизвестное значение -> сохранено прежнее (default true).
        assert!(cfg.auto_correct);
        // default_layout: неизвестная раскладка -> Keep.
        assert_eq!(cfg.default_layout, DefaultLayout::Keep);
        // idle_pause_seconds: не число -> дефолт.
        assert_eq!(cfg.idle_pause_seconds, Config::default().idle_pause_seconds);
    }

    #[test]
    fn test_parse_bool_garbage_keeps_previous() {
        // Опечатка в значении не должна молча выключать фичу.
        let cfg = Config::parse("typing_detect = ture\nretro_fix = flase\n");
        assert!(cfg.typing_detect);
        assert!(cfg.retro_fix);
        // Явные значения работают в обе стороны.
        let cfg = Config::parse("typing_detect = no\nretro_fix = 0\n");
        assert!(!cfg.typing_detect);
        assert!(!cfg.retro_fix);
    }

    #[test]
    fn test_excluded_apps_case_insensitive_unicode() {
        let cfg = Config::parse("exclude_apps = Терминал, 1Password\n");
        assert!(cfg.is_excluded("терминал"));
        assert!(cfg.is_excluded("ТЕРМИНАЛ"));
        assert!(cfg.is_excluded("1password"));
        assert!(!cfg.is_excluded("Safari"));
    }

    #[test]
    fn test_paths_live_in_user_config_dir() {
        // Конфиг и журнал — в одном месте: ~/.config/smartswitcher
        // (или $XDG_CONFIG_HOME/smartswitcher).
        let dir = config_dir();
        assert_eq!(Config::default_path(), dir.join("config.txt"));
        let in_home = dir
            .ancestors()
            .any(|a| a.file_name().is_some_and(|n| n == ".config"));
        assert!(
            in_home || std::env::var_os("XDG_CONFIG_HOME").is_some(),
            "каталог конфига должен лежать в ~/.config (или XDG): {dir:?}"
        );
        assert!(Config::default_path().is_absolute());
    }

    /// `assets/config.txt` — эталон для Makefile (копируется в target/* после
    /// cargo clean). Тест не даёт ему разойтись с `Config::default()`.
    #[test]
    fn test_repo_default_config_matches_defaults() {
        let parsed = Config::parse(include_str!("../assets/config.txt"));
        let d = Config::default();
        assert_eq!(parsed.auto_correct, d.auto_correct);
        assert_eq!(parsed.default_layout, d.default_layout);
        assert_eq!(parsed.idle_pause_seconds, d.idle_pause_seconds);
        assert_eq!(parsed.excluded_apps, d.excluded_apps);
        assert_eq!(parsed.typing_detect, d.typing_detect);
        assert_eq!(parsed.min_prefix_chars, d.min_prefix_chars);
        assert_eq!(parsed.switch_threshold, d.switch_threshold);
        assert_eq!(parsed.retro_fix, d.retro_fix);
        assert_eq!(parsed.convert_hotkey, d.convert_hotkey);
        assert_eq!(parsed.spell_suggest, d.spell_suggest);
        assert_eq!(parsed.spell_min_chars, d.spell_min_chars);
        assert_eq!(parsed.spell_max_suggestions, d.spell_max_suggestions);
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
            typing_detect: true,
            min_prefix_chars: 4,
            switch_threshold: 55.5,
            retro_fix: false,
            convert_hotkey: Hotkey::parse("ctrl+shift+f13").unwrap(),
            spell_suggest: false,
            spell_min_chars: 5,
            spell_max_suggestions: 7,
        };
        let reloaded = Config::parse(&original.to_file_string());
        assert_eq!(reloaded.auto_correct, original.auto_correct);
        assert_eq!(reloaded.default_layout, original.default_layout);
        assert_eq!(reloaded.idle_pause_seconds, original.idle_pause_seconds);
        assert_eq!(reloaded.excluded_apps, original.excluded_apps);
        assert_eq!(reloaded.typing_detect, original.typing_detect);
        assert_eq!(reloaded.min_prefix_chars, original.min_prefix_chars);
        assert_eq!(reloaded.switch_threshold, original.switch_threshold);
        assert_eq!(reloaded.retro_fix, original.retro_fix);
        assert_eq!(reloaded.convert_hotkey, original.convert_hotkey);
        assert_eq!(reloaded.spell_suggest, original.spell_suggest);
        assert_eq!(reloaded.spell_min_chars, original.spell_min_chars);
        assert_eq!(
            reloaded.spell_max_suggestions,
            original.spell_max_suggestions
        );
    }
}
