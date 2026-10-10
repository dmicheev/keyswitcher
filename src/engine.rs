//! Платформо-независимая логика «мозга» PuntoSwitcher.
//!
//! Ядро оперирует абстракциями [`Layout`] и трейтом [`System`], который
//! реализует доступ к ОС (активное приложение, текущая раскладка, её смена).
//! Благодаря этому вся бизнес-логика тестируется на любой платформе
//! (см. `src/stub.rs` и unit-тесты), а реализация для macOS находится в `src/macos/`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::{Config, DefaultLayout};

/// Раскладка клавиатуры, значимая для переключателя.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layout {
    /// Английская (US)
    En,
    /// Русская
    Ru,
    /// Любая иная — переключатель её не трогает.
    Other,
}

/// Ключ хранения состояния: идентификатор приложения + (опционально) окно.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowKey {
    pub app_id: String,
    pub window_title: Option<String>,
}

impl WindowKey {
    pub fn new(app_id: &str, window_title: Option<&str>) -> Self {
        Self {
            app_id: app_id.to_string(),
            window_title: window_title.map(|s| s.to_string()),
        }
    }
}

/// Информация об активном приложении/окне, получаемая от ОС.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    /// Bundle id или имя процесса (например, "com.apple.Safari" или "Safari").
    pub app_id: String,
    /// Человекочитаемое имя приложения.
    pub app_name: String,
    /// Заголовок активного окна (если доступен).
    pub title: Option<String>,
}

impl ActiveWindow {
    pub fn key(&self) -> WindowKey {
        WindowKey::new(&self.app_id, self.title.as_deref())
    }
}

/// Абстракция доступа к ОС. Реализации: macOS (cocoa/objc), тестовый заглушек.
pub trait System {
    /// Активное приложение и его окно. `None`, если определить не удалось.
    fn active_window(&mut self) -> Option<ActiveWindow>;
    /// Текущая раскладка клавиатуры.
    fn current_layout(&mut self) -> Layout;
    /// Принудительно применяет раскладку. Возвращает true при успехе.
    fn set_layout(&mut self, layout: Layout) -> bool;
}

/// Сохранённое состояние одного окна/приложения.
#[derive(Debug, Clone)]
struct SavedState {
    layout: Layout,
    saved_at: Instant,
}

/// Результат одного шага обработки — удобно для логов и тестов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ничего делать не нужно (та же раскладка, нет активного окна и т.п.)
    Nothing,
    /// Окно стало активным — восстановили его раскладку.
    Restored(Layout),
    /// Новое окно — применили раскладку по умолчанию.
    AppliedDefault(Layout),
    /// Пользователь сам сменил раскладку — запомнили новое состояние.
    Remembered(Layout),
    /// Приложение в списке исключений — не трогаем.
    SkippedExcluded,
}

/// Собственно переключатель: хранит историю раскладок по окнам.
pub struct Switcher<'a, S: System> {
    system: &'a mut S,
    config: Config,
    /// Последнее активное окно.
    last_window: Option<WindowKey>,
    /// Последняя «виденная» нами раскладка (для детекта ручного переключения).
    last_seen_layout: Layout,
    /// История: окно -> последняя использованная там раскладка.
    history: HashMap<WindowKey, SavedState>,
}

impl<'a, S: System> Switcher<'a, S> {
    pub fn new(system: &'a mut S, config: Config) -> Self {
        let initial_layout = system.current_layout();
        Self {
            system,
            config,
            last_window: None,
            last_seen_layout: initial_layout,
            history: HashMap::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Один шаг основного цикла: анализируем активное окно и при необходимости
    /// меняем раскладку. Возвращает выполненное действие.
    pub fn step(&mut self) -> Action {
        let Some(win) = self.system.active_window() else {
            return Action::Nothing;
        };
        let key = win.key();

        // 1. Приложение в исключениях — ничего не делаем, но обновляем «якорь».
        if self.config.is_excluded(&win.app_name) || self.config.is_excluded(&win.app_id) {
            self.last_window = Some(key);
            self.last_seen_layout = self.system.current_layout();
            return Action::SkippedExcluded;
        }

        // 2. Фиксируем ручную смену раскладки пользователем (без смены окна).
        let current = self.system.current_layout();
        if current != self.last_seen_layout && self.last_window.as_ref() == Some(&key) {
            self.remember(&key, current);
            self.last_seen_layout = current;
            return Action::Remembered(current);
        }

        // 3. Сосредоточены на новом окне?
        if self.last_window.as_ref() != Some(&key) {
            let action = self.on_window_changed(&key, current);
            self.last_window = Some(key);
            self.last_seen_layout = self.system.current_layout();
            return action;
        }

        self.last_seen_layout = current;
        Action::Nothing
    }

    fn on_window_changed(&mut self, key: &WindowKey, current: Layout) -> Action {
        // Есть ли свежее сохранённое состояние для этого окна?
        let fresh = match self.history.get(key) {
            Some(st) if st.saved_at.elapsed() < Duration::from_secs(self.config.idle_pause_seconds) => {
                Some(st.layout)
            }
            _ => None,
        };

        let target = match fresh {
            Some(l) if l != Layout::Other => {
                log::debug!("Восстановление раскладки {:?} для {:?}", l, key);
                l
            }
            _ => match self.config.default_layout {
                DefaultLayout::Keep => {
                    // Оставляем как есть, но запоминаем текущую для будущего восстановления.
                    self.remember(key, current);
                    return Action::Nothing;
                }
                DefaultLayout::En => DefaultLayout::En.into_layout(),
                DefaultLayout::Ru => DefaultLayout::Ru.into_layout(),
            },
        };

        if current != target {
            if self.system.set_layout(target) {
                self.remember(key, target);
                return Action::Restored(target);
            }
            log::warn!("Не удалось применить раскладку {:?} — возможно, она не добавлена в настройках системы", target);
            return Action::Nothing;
        }
        self.remember(key, target);
        Action::Nothing
    }

    fn remember(&mut self, key: &WindowKey, layout: Layout) {
        if layout == Layout::Other {
            return; // чужие раскладки не запоминаем
        }
        self.history.insert(
            key.clone(),
            SavedState {
                layout,
                saved_at: Instant::now(),
            },
        );
    }

    /// Полная перезагрузка конфигурации (например, по сигналу или из меню).
    pub fn reload_config(&mut self, config: Config) {
        self.set_config(config);
    }

    /// Текущая раскладка в системе (тесты/отладка).
    pub fn system_layout(&mut self) -> Layout {
        self.system.current_layout()
    }
}

trait IntoLayout {
    fn into_layout(self) -> Layout;
}
impl IntoLayout for DefaultLayout {
    fn into_layout(self) -> Layout {
        match self {
            DefaultLayout::En => Layout::En,
            DefaultLayout::Ru => Layout::Ru,
            DefaultLayout::Keep => Layout::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stub::StubSystem;

    fn cfg() -> Config {
        Config {
            auto_correct: true,
            default_layout: DefaultLayout::En,
            idle_pause_seconds: 3600,
            excluded_apps: ["Terminal".into()].into_iter().collect(),
        }
    }

    #[test]
    fn switches_layout_for_new_window_with_default_en() {
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru); // пользователь набирал по-русски в другом месте
        let mut sw = Switcher::new(&mut sys, cfg());

        // Первый шаг: окно неизвестно, дефолт EN -> должна сработать смена.
        let a = sw.step();
        assert_eq!(a, Action::Restored(Layout::En));
        assert_eq!(sw.system_layout(), Layout::En);
    }

    #[test]
    fn remembers_manual_switch_and_restores_on_return() {
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        let mut sw = Switcher::new(&mut sys, cfg());
        sw.step(); // Safari/Doc стала активной, применён дефолт EN

        // Пользователь вручную переключает раскладку на RU (в том же окне) —
        // переключатель запоминает это.
        sys.set_current(Layout::Ru);
        assert_eq!(sw.step(), Action::Remembered(Layout::Ru));

        // Переход в другое окно...
        sys.push_app("Notes", "List");
        sw.step();

        // ...и возврат на Safari: сохранённая RU восстанавливается.
        sys.reactivate(0);
        let a = sw.step();
        assert_eq!(a, Action::Restored(Layout::Ru));
    }

    #[test]
    fn respects_excluded_apps() {
        let mut sys = StubSystem::new();
        sys.push_app("Terminal", "zsh");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(&mut sys, cfg());
        assert_eq!(sw.step(), Action::SkippedExcluded);
        assert_eq!(sw.system_layout(), Layout::Ru); // не тронули
    }

    #[test]
    fn keep_default_does_not_touch_layout() {
        let mut c = cfg();
        c.default_layout = DefaultLayout::Keep;
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(&mut sys, c);
        assert_eq!(sw.step(), Action::Nothing);
        assert_eq!(sw.system_layout(), Layout::Ru);
    }
}
