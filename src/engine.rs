//! Платформо-независимая логика «мозга» SmartSwitcher (движок переключения).
//!
//! Ядро оперирует абстракциями [`Layout`] и трейтом [`System`], который
//! реализует доступ к ОС (активное приложение, текущая раскладка, её смена).
//! [`Switcher`] не владеет реализацией `System`: она передаётся в [`Switcher::step`],
//! поэтому движок можно крутить поверх любой системы и свободно вмешиваться
//! в её состояние (как это делают unit-тесты).
//! Реализация для macOS находится в `src/macos/`.

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

/// Абстракция доступа к ОС. Реализации: macOS (objc), тестовая заглушка.
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
    /// Окно стало активным — восстановили его сохранённую раскладку.
    Restored(Layout),
    /// Новое окно — применили раскладку по умолчанию.
    AppliedDefault(Layout),
    /// Пользователь сам сменил раскладку — запомнили новое состояние.
    Remembered(Layout),
    /// Приложение в списке исключений — не трогаем.
    SkippedExcluded,
}

/// Движок переключения: хранит историю раскладок по окнам.
/// Реализация `System` передаётся в методы, а не хранится внутри —
/// так состояние движка можно менять извне между шагами (тесты, отладка).
pub struct Switcher {
    config: Config,
    /// Последнее активное окно.
    last_window: Option<WindowKey>,
    /// Последняя «виденная» нами раскладка (`None` — ещё неизвестна).
    last_seen_layout: Option<Layout>,
    /// История: окно -> последняя использованная там раскладка.
    history: HashMap<WindowKey, SavedState>,
}

impl Switcher {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            last_window: None,
            last_seen_layout: None,
            history: HashMap::new(),
        }
    }

    /// Текущая конфигурация (UI показывает её в настройках).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Один шаг основного цикла: анализируем активное окно и при необходимости
    /// меняем раскладку. Возвращает выполненное действие.
    pub fn step<S: System>(&mut self, system: &mut S) -> Action {
        if !self.config.auto_correct {
            return Action::Nothing;
        }

        let Some(win) = system.active_window() else {
            return Action::Nothing;
        };
        let key = win.key();

        // 1. Приложение в исключениях — ничего не делаем, но обновляем «якорь».
        if self.config.is_excluded(&win.app_name) || self.config.is_excluded(&win.app_id) {
            self.last_window = Some(key);
            self.last_seen_layout = Some(system.current_layout());
            return Action::SkippedExcluded;
        }

        // 2. Фиксируем ручную смену раскладки пользователем (без смены окна).
        let current = system.current_layout();
        if Some(current) != self.last_seen_layout && self.last_window.as_ref() == Some(&key) {
            self.remember(&key, current);
            self.last_seen_layout = Some(current);
            return Action::Remembered(current);
        }

        // 3. Сосредоточены на новом окне?
        if self.last_window.as_ref() != Some(&key) {
            self.prune_history();
            let action = self.on_window_changed(system, &key, current);
            self.last_window = Some(key);
            self.last_seen_layout = Some(system.current_layout());
            return action;
        }

        self.last_seen_layout = Some(current);
        Action::Nothing
    }

    fn on_window_changed<S: System>(
        &mut self,
        system: &mut S,
        key: &WindowKey,
        current: Layout,
    ) -> Action {
        // Есть ли свежее сохранённое состояние для этого окна?
        // (remember() не сохраняет Layout::Other, поэтому fresh всегда En/Ru.)
        if let Some(target) = self.fresh_layout(key) {
            return self.apply(system, key, target, Action::Restored);
        }

        match self.config.default_layout {
            DefaultLayout::Keep => {
                // Оставляем как есть, но запоминаем текущую для будущего восстановления.
                self.remember(key, current);
                Action::Nothing
            }
            DefaultLayout::En => self.apply(system, key, Layout::En, Action::AppliedDefault),
            DefaultLayout::Ru => self.apply(system, key, Layout::Ru, Action::AppliedDefault),
        }
    }

    /// Применяет `target`, если текущая раскладка отличается. `on_success`
    /// различает восстановление сохранённой раскладки и применение дефолтной.
    fn apply<S: System>(
        &mut self,
        system: &mut S,
        key: &WindowKey,
        target: Layout,
        on_success: fn(Layout) -> Action,
    ) -> Action {
        let current = system.current_layout();
        if current == target {
            self.remember(key, target);
            return Action::Nothing;
        }
        if system.set_layout(target) {
            self.remember(key, target);
            return on_success(target);
        }
        log::warn!(
            "Не удалось применить раскладку {:?} — возможно, она не добавлена в настройках системы",
            target
        );
        Action::Nothing
    }

    /// Свежая сохранённая раскладка окна, если она ещё не «протухла».
    fn fresh_layout(&self, key: &WindowKey) -> Option<Layout> {
        let state = self.history.get(key)?;
        let ttl = Duration::from_secs(self.config.idle_pause_seconds);
        (state.saved_at.elapsed() < ttl).then_some(state.layout)
    }

    /// Выбрасывает «протухшие» записи — окно с истёкшим состоянием всё равно
    /// получило бы дефолт, так что очистка не меняет поведение, но не даёт
    /// истории расти неограниченно (браузерные вкладки и т.п.).
    fn prune_history(&mut self) {
        let ttl = Duration::from_secs(self.config.idle_pause_seconds);
        self.history.retain(|_, st| st.saved_at.elapsed() < ttl);
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

    /// Перезагрузка конфигурации «на горячую» (основной цикл делает это по таймеру).
    pub fn reload_config(&mut self, config: Config) {
        self.config = config;
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
            ..Config::default()
        }
    }

    #[test]
    fn switches_layout_for_new_window_with_default_en() {
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru); // пользователь набирал по-русски в другом месте
        let mut sw = Switcher::new(cfg());

        // Первый шаг: окно неизвестно, дефолт EN -> должна сработать смена.
        let a = sw.step(&mut sys);
        assert_eq!(a, Action::AppliedDefault(Layout::En));
        assert_eq!(sys.current_layout(), Layout::En);
    }

    #[test]
    fn remembers_manual_switch_and_restores_on_return() {
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        let mut sw = Switcher::new(cfg());
        sw.step(&mut sys); // Safari/Doc стала активной, применён дефолт EN

        // Пользователь вручную переключает раскладку на RU (в том же окне) —
        // переключатель запоминает это.
        sys.set_current(Layout::Ru);
        assert_eq!(sw.step(&mut sys), Action::Remembered(Layout::Ru));

        // Переход в другое окно...
        sys.push_app("Notes", "List");
        sw.step(&mut sys);

        // ...и возврат на Safari: сохранённая RU восстанавливается.
        sys.reactivate(0);
        let a = sw.step(&mut sys);
        assert_eq!(a, Action::Restored(Layout::Ru));
    }

    #[test]
    fn respects_excluded_apps() {
        let mut sys = StubSystem::new();
        sys.push_app("Terminal", "zsh");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(cfg());
        assert_eq!(sw.step(&mut sys), Action::SkippedExcluded);
        assert_eq!(sys.current_layout(), Layout::Ru); // не тронули
    }

    #[test]
    fn keep_default_does_not_touch_layout() {
        let mut c = cfg();
        c.default_layout = DefaultLayout::Keep;
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(c);
        assert_eq!(sw.step(&mut sys), Action::Nothing);
        assert_eq!(sys.current_layout(), Layout::Ru);
    }

    #[test]
    fn auto_correct_disabled_is_noop() {
        let mut c = cfg();
        c.auto_correct = false;
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(c);
        assert_eq!(sw.step(&mut sys), Action::Nothing);
        assert_eq!(sys.current_layout(), Layout::Ru);
    }

    #[test]
    fn applied_default_and_restored_are_distinguishable() {
        let mut sys = StubSystem::new();
        sys.push_app("Safari", "Doc");
        sys.set_current(Layout::Ru);
        let mut sw = Switcher::new(cfg());

        // Новое окно — дефолт.
        assert_eq!(sw.step(&mut sys), Action::AppliedDefault(Layout::En));

        // Ручное переключение запоминается...
        sys.set_current(Layout::Ru);
        assert_eq!(sw.step(&mut sys), Action::Remembered(Layout::Ru));

        // ...другое новое окно — снова дефолт...
        sys.push_app("Notes", "List");
        assert_eq!(sw.step(&mut sys), Action::AppliedDefault(Layout::En));

        // ...а возврат в окно с сохранённым состоянием — именно восстановление.
        sys.reactivate(0);
        assert_eq!(sw.step(&mut sys), Action::Restored(Layout::Ru));
    }

    #[test]
    fn reload_config_updates_exclusions() {
        let mut sys = StubSystem::new();
        sys.push_app("Terminal", "zsh");
        let mut sw = Switcher::new(Config::default()); // исключений нет, Keep
        assert_eq!(sw.step(&mut sys), Action::Nothing);

        sw.reload_config(cfg()); // в cfg() Terminal исключён
        sys.push_app("Terminal", "zsh2");
        assert_eq!(sw.step(&mut sys), Action::SkippedExcluded);
    }
}
