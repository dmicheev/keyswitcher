//! Реализация трейта [`System`] для macOS.
//!
//! * Активное приложение — через `NSWorkspace.sharedWorkspace.frontmostApplication`.
//! * Текущая раскладка и её смена — через Carbon TIS (см. [`super::ffi`]).
//! * Заголовок активного окна — через Accessibility API (C-мост `ax_bridge`).
//!   Для этого пользователю нужно выдать приложению разрешение
//!   «Системные настройки → Конфиденциальность и безопасность → Доступность».
//!   Без разрешения переключатель корректно деградирует до режима
//!   «раскладка на уровне приложения».

// Макросы objc 0.2 (msg_send!/class!) внутри используют cfg(feature = "cargo-clippy")
// — на новых тулчейнах это даёт предупреждение unexpected_cfgs. Известная
// особенность старого objc, глушим точечно.
#![allow(unexpected_cfgs)]

use core::ffi::c_char;
use std::ptr;

use objc::runtime::Object;
// `sel`/`sel_impl` нужны самой развёртке макроса msg_send!.
use objc::{class, msg_send, sel, sel_impl};

use crate::engine::{ActiveWindow, Layout, System};
use crate::macos::ffi;

type Id = *mut Object;

const NIL: Id = ptr::null_mut();

/// RAII-обёртка над NSAutoreleasePool: drain при выходе из области видимости.
pub(crate) struct AutoReleasePool(Id);

impl AutoReleasePool {
    pub(crate) fn new() -> Self {
        // Safety: вызывается на main-потоке с живым AppKit-рантаймом.
        AutoReleasePool(unsafe { msg_send![class!(NSAutoreleasePool), new] })
    }
}

impl Drop for AutoReleasePool {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // Safety: объект создан нами в `new`.
            unsafe {
                let _: () = msg_send![self.0, drain];
            }
        }
    }
}

/// Кандидаты-раскладки EN/RU из числа ВКЛЮЧЁННЫХ источников ввода.
/// Хранится список: если выбор первого по какой-то причине не удался,
/// переключатель пробует следующий.
#[derive(Default)]
struct KnownSources {
    en_ids: Vec<String>,
    ru_ids: Vec<String>,
    /// Итог сканирования уже записан в журнал (не спамим при повторных сканах).
    reported: bool,
}

pub struct MacSystem {
    known: KnownSources,
}

impl Default for MacSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl MacSystem {
    pub fn new() -> Self {
        Self {
            known: KnownSources::default(),
        }
    }

    /// Инициализация NSApplication в режиме агента (без иконки в Dock).
    /// Должно вызываться на main-потоке до запуска цикла.
    pub fn setup_ns_application() {
        let _pool = AutoReleasePool::new();
        // NSApplicationActivationPolicyAccessory = 1 — агент без Dock-иконки.
        unsafe {
            let app: Id = msg_send![class!(NSApplication), sharedApplication];
            if app != NIL {
                let _: () = msg_send![app, setActivationPolicy: 1isize];
            }
        }
    }

    /// Печатает включённые источники ввода и их классификацию (`--list-layouts`).
    pub fn print_input_sources() {
        println!("Включённые источники ввода (классификация переключателя):");
        // Safety: CF-вызовы только на чтение.
        unsafe {
            let list = ffi::TISCreateInputSourceList(ptr::null(), 0);
            if list.is_null() {
                println!("  <не удалось получить список>");
                return;
            }
            let count = ffi::CFArrayGetCount(list);
            if count == 0 {
                println!("  <список пуст>");
            }
            for i in 0..count {
                let src = ffi::CFArrayGetValueAtIndex(list, i) as ffi::TISInputSourceRef;
                if src.is_null() {
                    continue;
                }
                let sid = ffi::input_source_id(src).unwrap_or_else(|| "<без ID>".into());
                println!("  {:<45} {:?}", sid, Self::classify_source(src));
            }
            ffi::CFRelease(list as ffi::CFTypeRef);
        }
    }

    /// Классификация источника по ID — статический фолбек для случая, когда
    /// раскладка не отдаёт свои данные (`kTISPropertyUnicodeKeyLayoutData`).
    pub(crate) fn classify(id_str: &str) -> Layout {
        let s = id_str.to_ascii_lowercase();
        // Управляем только клавиатурными раскладками; методы ввода
        // (inputmode: CJK-режимы, палитры и т.п.) не трогаем — иначе,
        // например, китайский режим "...ITABC" ложно классифицируется как EN.
        if !s.contains("keylayout") {
            return Layout::Other;
        }
        if s.contains("russian") || s.contains("cyrillic") {
            Layout::Ru
        } else if s.contains(".us")
            || s.ends_with("us")
            || s.contains("british")
            || s.contains("english")
            || s.contains("abc")
            || s.contains("canad")
            || s.contains("csa")
        {
            Layout::En
        } else {
            Layout::Other
        }
    }

    /// Классификация источника ввода «как он есть»: сначала по скрипту
    /// символов, которые раскладка реально печатает (UCKeyTranslate —
    /// любые латинские/кириллические раскладки, включая канадскую),
    /// фолбек — по ID ([`Self::classify`]).
    ///
    /// # Safety
    /// `src` — валидный TISInputSourceRef (main-поток).
    pub(crate) unsafe fn classify_source(src: ffi::TISInputSourceRef) -> Layout {
        // Методы ввода (не keylayout) не трогаем даже при наличии у них
        // данных раскладки — иначе CJK-режимы ложно уйдут в EN.
        let sid = ffi::input_source_id(src).unwrap_or_default();
        if !sid.to_ascii_lowercase().contains("keylayout") {
            return Layout::Other;
        }
        if let Some(layout) = crate::macos::keymap::script_layout(src) {
            return layout;
        }
        Self::classify(&sid)
    }

    /// Сканирует ВКЛЮЧЁННЫЕ источники ввода и собирает ID раскладок EN/RU.
    /// Повторяется, пока не найдётся и та, и другая (пользователь может
    /// добавить раскладку в настройках на горячую).
    fn refresh_known_sources(&mut self) {
        if !self.known.en_ids.is_empty() && !self.known.ru_ids.is_empty() {
            return;
        }
        // Safety: Carbon/CF-вызовы, выполняются на main-потоке.
        unsafe {
            // allSources = 0: только включённые — выключенная раскладка
            // (например, US при включённом ABC) не выбирается через TIS.
            let list = ffi::TISCreateInputSourceList(ptr::null(), 0);
            if list.is_null() {
                return;
            }
            let count = ffi::CFArrayGetCount(list);
            for i in 0..count {
                let src = ffi::CFArrayGetValueAtIndex(list, i) as ffi::TISInputSourceRef;
                if src.is_null() {
                    continue;
                }
                let Some(sid) = ffi::input_source_id(src) else {
                    continue;
                };
                match Self::classify_source(src) {
                    Layout::En if !self.known.en_ids.contains(&sid) => self.known.en_ids.push(sid),
                    Layout::Ru if !self.known.ru_ids.contains(&sid) => self.known.ru_ids.push(sid),
                    _ => {}
                }
            }
            ffi::CFRelease(list as ffi::CFTypeRef);
        }
        if !self.known.reported {
            self.known.reported = true;
            log::info!(
                "Включённые раскладки: EN={:?}, RU={:?}",
                self.known.en_ids,
                self.known.ru_ids
            );
            if self.known.en_ids.is_empty() {
                log::warn!(
                    "EN-раскладка не найдена среди включённых источников ввода \
                     (System Settings → Keyboard → Input Sources)"
                );
            }
            if self.known.ru_ids.is_empty() {
                log::warn!("RU-раскладка не найдена среди включённых источников ввода");
            }
        }
    }

    fn select_source_by_id(&self, source_id: &str) -> bool {
        // Safety: Carbon/CF-вызовы, выполняются на main-потоке.
        unsafe {
            let Some((src, list)) = ffi::find_input_source(source_id) else {
                return false;
            };
            let status = ffi::TISSelectInputSource(src);
            ffi::CFRelease(list as ffi::CFTypeRef);
            status == 0
        }
    }
}

impl System for MacSystem {
    fn active_window(&mut self) -> Option<ActiveWindow> {
        let _pool = AutoReleasePool::new();
        // Safety: AppKit-вызовы на main-потоке; указатели проверяются на nil/null.
        unsafe {
            let ws: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
            if ws == NIL {
                return None;
            }
            let app: Id = msg_send![ws, frontmostApplication];
            if app == NIL {
                return None;
            }
            let bundle_id: Id = msg_send![app, bundleIdentifier];
            let name: Id = msg_send![app, localizedName];

            let app_name = nsstring_to_string(name)?;
            let app_id = nsstring_to_string(bundle_id).unwrap_or_else(|| app_name.clone());

            let pid: i64 = msg_send![app, processIdentifier];
            let title = ax_bridge_title(pid);

            Some(ActiveWindow {
                app_id,
                app_name,
                title,
            })
        }
    }

    fn current_layout(&mut self) -> Layout {
        self.refresh_known_sources();
        // Safety: TISCopyCurrentKeyboardInputSource возвращает +1 retain.
        unsafe {
            let src = ffi::TISCopyCurrentKeyboardInputSource();
            if src.is_null() {
                return Layout::Other;
            }
            let layout = Self::classify_source(src);
            ffi::CFRelease(src as ffi::CFTypeRef);
            layout
        }
    }

    fn set_layout(&mut self, layout: Layout) -> bool {
        self.refresh_known_sources();
        let candidates: &[String] = match layout {
            Layout::En => &self.known.en_ids,
            Layout::Ru => &self.known.ru_ids,
            Layout::Other => &[],
        };
        // Пробуем кандидатов по очереди: первый может не выбраться
        // (например, добавлен, но ещё не активирован).
        candidates.iter().any(|id| self.select_source_by_id(id))
    }
}

// ---------------------------------------------------------------------------
// Мост к Accessibility API (C, см. assets/ax_bridge.c, линкуется build.rs).
// ---------------------------------------------------------------------------
extern "C" {
    /// Заголовок фокусного окна процесса `pid` (malloc'ная UTF-8 строка) или NULL.
    fn ax_bridge_copy_title(pid: i32) -> *const c_char;
    fn ax_bridge_free(ptr: *const c_char);
    /// Позиция указателя мыши в экранных координатах CG/AX.
    fn ax_bridge_mouse_location(out_x: *mut f64, out_y: *mut f64) -> i32;
    /// Геометрия дисплея с точкой + высота главного дисплея (для флипа Y).
    fn ax_bridge_display_geometry(
        px: f64,
        py: f64,
        out_flip_h: *mut f64,
        out_dx: *mut f64,
        out_dy: *mut f64,
        out_dw: *mut f64,
        out_dh: *mut f64,
    ) -> i32;
}

/// Позиция указателя мыши `(x, y)` в «экранных» координатах CG/AX
/// (origin — левый верхний угол главного монитора, y растёт вниз).
/// `None` — получить не удалось (экзотика).
pub(crate) fn ax_mouse_location() -> Option<(f64, f64)> {
    let (mut x, mut y) = (0f64, 0f64);
    // Safety: C-функция только пишет в переданные указатели.
    let ok = unsafe { ax_bridge_mouse_location(&mut x, &mut y) };
    (ok == 1).then_some((x, y))
}

/// Геометрия дисплеев для позиционирования попапа: `(flip_h, dx, dy, dw, dh)`.
///
/// `flip_h` — высота главного дисплея: перевод `y` из «экранных» координат
/// AX (от верхнего края) в AppKit (от нижнего) всегда выполняется по нему,
/// независимо от того, на каком дисплее находится точка. `(dx, dy, dw, dh)` —
/// bounds дисплея, содержащего точку `(px, py)` (кламп панели).
/// `None` — список дисплеев недоступен (экзотика).
pub(crate) fn ax_display_geometry(px: f64, py: f64) -> Option<(f64, f64, f64, f64, f64)> {
    let (mut flip_h, mut dx, mut dy, mut dw, mut dh) = (0f64, 0f64, 0f64, 0f64, 0f64);
    // Safety: C-функция только пишет в переданные указатели.
    let ok = unsafe {
        ax_bridge_display_geometry(px, py, &mut flip_h, &mut dx, &mut dy, &mut dw, &mut dh)
    };
    (ok == 1).then_some((flip_h, dx, dy, dw, dh))
}

/// # Safety: указатель из ax_bridge_copy_title валиден до ax_bridge_free.
unsafe fn ax_bridge_title(pid: i64) -> Option<String> {
    let p = ax_bridge_copy_title(pid as i32);
    if p.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
    ax_bridge_free(p);
    (!s.is_empty()).then_some(s)
}

/// # Safety: `s` должен быть валидным указателем на NSString или NULL.
pub(crate) unsafe fn nsstring_to_string(s: Id) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let utf8: *const c_char = msg_send![s, UTF8String];
    if utf8.is_null() {
        return None;
    }
    // Копируем в собственную строку: данные за указателем живут до drain пула.
    Some(
        std::ffi::CStr::from_ptr(utf8)
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_common_input_source_ids() {
        assert_eq!(MacSystem::classify("com.apple.keylayout.ABC"), Layout::En);
        assert_eq!(MacSystem::classify("com.apple.keylayout.US"), Layout::En);
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.British-PC"),
            Layout::En
        );
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.Russian"),
            Layout::Ru
        );
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.RussianWin"),
            Layout::Ru
        );
        // Канадские раскладки — латинская сторона пары (ID-фолбек;
        // основной путь — скрипт-детект по UCKeyTranslate).
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.Canadian"),
            Layout::En
        );
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.CanadianEnglish"),
            Layout::En
        );
        assert_eq!(
            MacSystem::classify("com.apple.keylayout.CanadianFrench"),
            Layout::En
        );
        assert_eq!(MacSystem::classify("com.apple.keylayout.CSA"), Layout::En);
        // Методы ввода (не keylayout) не классифицируем — даже если в ID
        // встречается "abc".
        assert_eq!(
            MacSystem::classify("com.apple.inputmode.SCIM.ITABC"),
            Layout::Other
        );
        assert_eq!(
            MacSystem::classify("com.apple.CharacterPaletteIM"),
            Layout::Other
        );
    }
}
