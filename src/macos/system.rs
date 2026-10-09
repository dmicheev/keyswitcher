//! Реализация трейта [`System`] для macOS.
//!
//! * Активное приложение — через `NSWorkspace.sharedWorkspace.frontmostApplication`.
//! * Текущая раскладка и её смена — через Carbon TIS (см. [`super::ffi`]).
//! * Заголовок активного окна — через Accessibility API (C-мост `ax_bridge`).
//!   Для этого пользователю нужно выдать приложению разрешение
//!   «Системные настройки → Конфиденциальность и безопасность → Доступность».
//!   Без разрешения переключатель корректно деградирует до режима
//!   «раскладка на уровне приложения» (как «Исключения по программе» в Punto Switcher).

use cocoa::base::{id, nil};
use cocoa::foundation::NSAutoreleasePool;
use objc::runtime::BOOL;
use objc::{class, msg_send, sel, sel_impl};

use crate::core::{ActiveWindow, Layout, System};
use crate::macos::ffi;

/// Известные ID раскладок EN/RU, добавленных пользователем в системе.
#[derive(Default)]
struct KnownSources {
    en_id: Option<String>,
    ru_id: Option<String>,
}

pub struct MacSystem {
    known: KnownSources,
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
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let app: id = msg_send![class!(NSApplication), sharedApplication];
            // NSApplicationAccessory = 1 — агент без Dock-иконки.
            let _: () = msg_send![app, setActivationPolicy: 1isize];
        }
    }

    /// Классификация источника ввода по его ID.
    fn classify(id_str: &str) -> Layout {
        let s = id_str.to_ascii_lowercase();
        if s.contains("russian") || s.contains("cyrillic") {
            Layout::Ru
        } else if s.contains(".us")
            || s.ends_with("us")
            || s.contains("british")
            || s.contains("english")
            || s.contains("abc")
        {
            Layout::En
        } else {
            Layout::Other
        }
    }

    /// Один раз сканирует список источников ввода системы и запоминает
    /// подходящие ID английской и русской раскладок.
    unsafe fn refresh_known_sources(&mut self) {
        if self.known.en_id.is_some() && self.known.ru_id.is_some() {
            return;
        }
        let list = ffi::TISCreateInputSourceList(core::ptr::null(), 1);
        if list.is_null() {
            return;
        }
        let count = ffi::CFArrayGetCount(list);
        for i in 0..count {
            let src = ffi::CFArrayGetValueAtIndex(list, i) as ffi::TISInputSourceRef;
            if src.is_null() {
                continue;
            }
            let key = match ffi::string_to_cfstring(ffi::K_TIS_PROPERTY_ID) {
                Some(k) => k,
                None => continue,
            };
            let prop = ffi::TISGetInputSourceProperty(src, key);
            let sid = if prop.is_null() {
                None
            } else {
                ffi::cfstring_to_string(prop as ffi::CFStringRef)
            };
            ffi::CFRelease(key);
            if let Some(sid) = sid {
                match Self::classify(&sid) {
                    Layout::En if self.known.en_id.is_none() => self.known.en_id = Some(sid),
                    Layout::Ru if self.known.ru_id.is_none() => self.known.ru_id = Some(sid),
                    _ => {}
                }
            }
        }
        ffi::CFRelease(list as ffi::CFTypeRef);
    }

    unsafe fn select_source_by_id(&self, source_id: &str) -> bool {
        let Some((src, list)) = ffi::find_input_source(source_id) else {
            return false;
        };
        let status = ffi::TISSelectInputSource(src);
        ffi::CFRelease(list as ffi::CFTypeRef);
        status == 0
    }
}

impl System for MacSystem {
    fn active_window(&mut self) -> Option<ActiveWindow> {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);

            let ws: id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let app: id = msg_send![ws, frontmostApplication];
            if app == nil {
                return None;
            }
            let bundle_id: id = msg_send![app, bundleIdentifier];
            let name: id = msg_send![app, localizedName];

            let app_name = nsstring_to_rust(name)?.into_owned();
            let app_id = nsstring_to_rust(bundle_id)
                .map(|s| s.into_owned())
                .unwrap_or_else(|| app_name.clone());

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
        unsafe {
            self.refresh_known_sources();
            match ffi::current_input_source_id() {
                Some(id) => Self::classify(&id),
                None => Layout::Other,
            }
        }
    }

    fn set_layout(&mut self, layout: Layout) -> bool {
        unsafe {
            self.refresh_known_sources();
            let target_id = match layout {
                Layout::En => self.known.en_id.clone(),
                Layout::Ru => self.known.ru_id.clone(),
                Layout::Other => None,
            };
            match target_id {
                Some(id) => self.select_source_by_id(&id),
                None => false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Мост к Accessibility API (C, см. assets/ax_bridge.c, линкуется build.rs).
// ---------------------------------------------------------------------------
extern "C" {
    /// Заголовок фокусного окна процесса `pid` (malloc'ная UTF-8 строка) или NULL.
    fn ax_bridge_copy_title(pid: i32) -> *const std::os::raw::c_char;
    fn ax_bridge_free(ptr: *const std::os::raw::c_char);
}

unsafe fn ax_bridge_title(pid: i64) -> Option<String> {
    let p = ax_bridge_copy_title(pid as i32);
    if p.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
    ax_bridge_free(p);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

unsafe fn nsstring_to_rust<'a>(s: id) -> Option<std::borrow::Cow<'a, str>> {
    if s == nil {
        return None;
    }
    let utf8: *const std::os::raw::c_char = msg_send![s, UTF8String];
    if utf8.is_null() {
        return None;
    }
    Some(std::ffi::CStr::from_ptr(utf8).to_string_lossy())
}

/// Маркер использования типа BOOL из objc-runtime (пригодится при расширении).
#[allow(dead_code)]
fn _bool_marker(_: BOOL) {}
