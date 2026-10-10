//! Низкоуровневые обёртки над Carbon / TIS API (Text Input Source).
//!
//! Используется для определения и принудительной смены раскладки клавиатуры.
//! Код компилируется только под macOS.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use core::ffi::{c_char, c_void};

pub type TISInputSourceRef = *mut c_void;
pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFIndex = isize;
pub type OSStatus = i32;
pub type Boolean = u8;

extern "C" {
    // ---- CoreFoundation ----
    pub fn CFStringGetCStringPtr(theString: CFStringRef, encoding: u32) -> *const c_char;
    pub fn CFStringGetCString(
        theString: CFStringRef,
        buffer: *mut c_char,
        bufferSize: CFIndex,
        encoding: u32,
    ) -> Boolean;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(theArray: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(theArray: CFArrayRef, idx: CFIndex) -> *const c_void;

    // ---- Carbon HIToolbox / Text Input Source ----
    // Ключи свойств TIS — экспортируемые символы HIToolbox, а не строки:
    // линкуем их напрямую.
    pub static kTISPropertyInputSourceID: CFStringRef;

    pub fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    pub fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *mut c_void;
    pub fn TISCreateInputSourceList(properties: CFStringRef, allSources: Boolean) -> CFArrayRef;
    pub fn TISSelectInputSource(source: TISInputSourceRef) -> OSStatus;
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

/// Безопасное чтение CFStringRef в Rust String.
///
/// # Safety
/// Вызывающий обязан передать валидный ненулевой `CFStringRef`.
pub unsafe fn cfstring_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // Быстрая дорожка: указатель на UTF-8 напрямую.
    let p = CFStringGetCStringPtr(s, kCFStringEncodingUTF8);
    if !p.is_null() {
        return Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned());
    }
    // Медленная дорожка: копируем в буфер.
    let len = 1024;
    let mut buf = vec![0u8; len];
    let ok = CFStringGetCString(
        s,
        buf.as_mut_ptr() as *mut c_char,
        len as CFIndex,
        kCFStringEncodingUTF8,
    );
    (ok == 1).then(|| {
        let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char);
        cstr.to_string_lossy().into_owned()
    })
}

/// Строковый ID источника ввода (раскладки), либо None.
///
/// # Safety
/// `src` должен быть валидным TISInputSourceRef.
pub unsafe fn input_source_id(src: TISInputSourceRef) -> Option<String> {
    let prop = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
    if prop.is_null() {
        return None;
    }
    cfstring_to_string(prop as CFStringRef)
}

/// Возвращает строковый ID текущего источника ввода (раскладки), либо None.
///
/// # Safety
/// Требует корректно инициализированного Carbon-окружения (main-поток с NSRunLoop).
pub unsafe fn current_input_source_id() -> Option<String> {
    let src = TISCopyCurrentKeyboardInputSource();
    if src.is_null() {
        return None;
    }
    let id = input_source_id(src);
    // TISCopyCurrentKeyboardInputSource возвращает объект с +1 retain — обязаны освободить.
    CFRelease(src as CFTypeRef);
    id
}

/// Ищет включённый источник ввода с заданным ID.
///
/// # Safety
/// См. [`current_input_source_id`]. Возвращаемый ref принадлежит созданному
/// списку — его нельзя освобождать отдельно от списка.
pub unsafe fn find_input_source(id: &str) -> Option<(TISInputSourceRef, CFArrayRef)> {
    // allSources = 0: только ВКЛЮЧЁННЫЕ пользователем источники —
    // TISSelectInputSource гарантированно работает лишь для них.
    let list = TISCreateInputSourceList(core::ptr::null(), 0);
    if list.is_null() {
        return None;
    }
    let count = CFArrayGetCount(list);
    for i in 0..count {
        let src = CFArrayGetValueAtIndex(list, i) as TISInputSourceRef;
        if src.is_null() {
            continue;
        }
        if input_source_id(src).as_deref() == Some(id) {
            return Some((src, list));
        }
    }
    CFRelease(list as CFTypeRef);
    None
}
