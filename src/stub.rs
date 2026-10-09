//! Тестовая заглушка [`System`] — эмуляция ОС для unit-тестов ядра.
//! Работает на любой платформе, не требует доступа к реальной клавиатуре.

use crate::core::{ActiveWindow, Layout, System};

pub struct StubSystem {
    stack: Vec<ActiveWindow>,
    current: Layout,
    /// Все попытки смены раскладки (для проверок в тестах).
    pub applied: Vec<Layout>,
}

impl StubSystem {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            current: Layout::En,
            applied: Vec::new(),
        }
    }

    /// Делает приложение+окно активным (кладёт наверх стека).
    pub fn push_app(&mut self, app: &str, title: &str) {
        // Если такое окно уже есть в «истории» — просто поднимаем его,
        // иначе добавляем новое сверху.
        if let Some(pos) = self
            .stack
            .iter()
            .rposition(|w| w.app_name == app && w.title.as_deref() == Some(title))
        {
            let win = self.stack.remove(pos);
            self.stack.push(win);
            return;
        }
        self.stack.push(ActiveWindow {
            app_id: app.to_string(),
            app_name: app.to_string(),
            title: Some(title.to_string()),
        });
    }

    /// Делает активным окно с указанным индексом в стеке (эмуляция Alt+Tab).
    pub fn reactivate(&mut self, index: usize) {
        if index < self.stack.len() && index != self.stack.len() - 1 {
            let win = self.stack.remove(index);
            self.stack.push(win);
        }
    }

    /// Возврат на предыдущее («предпоследнее») окно — как Alt+Tab назад.
    pub fn pop_to_last_activated(&mut self) {
        if self.stack.len() >= 2 {
            let last = self.stack.remove(self.stack.len() - 2);
            self.stack.push(last);
        }
    }

    pub fn set_current(&mut self, l: Layout) {
        self.current = l;
    }
}

impl System for StubSystem {
    fn active_window(&mut self) -> Option<ActiveWindow> {
        self.stack.last().cloned()
    }

    fn current_layout(&mut self) -> Layout {
        self.current
    }

    fn set_layout(&mut self, layout: Layout) -> bool {
        self.applied.push(layout);
        self.current = layout;
        true
    }
}
