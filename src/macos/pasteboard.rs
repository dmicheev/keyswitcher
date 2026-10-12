//! Обёртка над NSPasteboard для конвертации выделенного текста
//! «скопировать → заменить → вставить»: работает в любых приложениях,
//! включая те, что не отдают выделение через Accessibility (Terminal,
//! Electron-приложения и т.п.).
//!
//! Все вызовы — только main-поток (как и весь конвейер тапа).

// Макросы objc 0.2 дают unexpected_cfgs на новых тулчейнах — см. system.rs.
#![allow(unexpected_cfgs)]

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

use super::system::{nsstring_to_string, AutoReleasePool};

type Id = *mut Object;

const NIL: Id = core::ptr::null_mut();
/// NSUTF8StringEncoding.
const NS_UTF8: isize = 4;
/// NSPasteboardTypeString.
const TYPE_STRING: &str = "public.utf8-plain-text";

fn general_pasteboard() -> Id {
    // Safety: AppKit-вызов на main-потоке.
    unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
}

/// # Safety
/// Возвращает autorelease-строку: валидна до drain текущего пула.
unsafe fn nsstring_from(s: &str) -> Id {
    let ns: Id = msg_send![class!(NSString), alloc];
    let ns: Id = msg_send![
        ns,
        initWithBytes: s.as_ptr()
        length: s.len()
        encoding: NS_UTF8
    ];
    if ns == NIL {
        return NIL;
    }
    let _: Id = msg_send![ns, autorelease];
    ns
}

/// Счётчик изменений буфера обмена (`None` — буфер недоступен).
pub(crate) fn change_count() -> Option<isize> {
    let _pool = AutoReleasePool::new();
    let pb = general_pasteboard();
    if pb == NIL {
        return None;
    }
    let count: isize = unsafe { msg_send![pb, changeCount] };
    Some(count)
}

/// Строковое содержимое буфера обмена. `None` — буфер пуст, недоступен
/// или содержит не строку (картинки, файлы — восстановить их мы не умеем).
pub(crate) fn string() -> Option<String> {
    let _pool = AutoReleasePool::new();
    let pb = general_pasteboard();
    if pb == NIL {
        return None;
    }
    // Safety: NSString создан нами, тип проверяется в nsstring_to_string.
    unsafe {
        let ty = nsstring_from(TYPE_STRING);
        let s: Id = msg_send![pb, stringForType: ty];
        nsstring_to_string(s)
    }
}

/// Записывает строку в буфер обмена (очищая прежнее содержимое).
pub(crate) fn set_string(s: &str) -> bool {
    let _pool = AutoReleasePool::new();
    let pb = general_pasteboard();
    if pb == NIL {
        return false;
    }
    // Safety: NSString создан нами (autorelease), вызовы AppKit на main-потоке.
    unsafe {
        let content = nsstring_from(s);
        if content == NIL {
            return false;
        }
        let ty = nsstring_from(TYPE_STRING);
        let _: bool = msg_send![pb, clearContents];
        let ok: bool = msg_send![pb, setString: content forType: ty];
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_roundtrip() {
        // Тест трогает буфер обмена — сохраняем и возвращаем строку на место.
        let old = string();
        let old_count = change_count();
        assert!(change_count().is_some(), "NSPasteboard доступен");
        assert!(set_string("smartswitcher-roundtrip-Ωм"));
        assert_eq!(string().as_deref(), Some("smartswitcher-roundtrip-Ωм"));
        assert_ne!(change_count(), old_count);
        if let Some(old) = old {
            assert!(set_string(&old));
        }
    }
}
