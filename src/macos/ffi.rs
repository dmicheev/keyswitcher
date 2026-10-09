//! Низкоуровневые обёрнки над Carbon / TIS API (Text Input Source).
//!
//! Используется для определения и принудительной смены раскладки клавиатуры.
//! Код компилируется только под macOS.

#![allow(non_snake_case, non_camel_case_types)]

use core::ffi::{c_char, c_void};

pub type TISInputSourceRef = *mut c_void;
pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFIndex = isize;
pub type OptionBits = u32;
pub type OSStatus = i32;
pub type Boolean = u8;

pub const K_TIS_PROPERTY_ID: &str = "TISPropertyID";
pub const K_TIS_PROPERTY_INPUT_SOURCE_TYPE: &str = "TISPropertyInputSourceType";
pub const K_TIS_PROPERTY_INPUT_SOURCE_CATEGORY: &str = "TISPropertyInputSourceCategory";
pub const K_TIS_PROPERTY_IS_SELECTABLE: &str = "TISPropertyIsSelectable";
pub const K_TIS_CATEGORY_KEYBOARD: &str = "TISCategoryKeyboardInputSource";
pub const K_TIS_INPUTSOURCE_TYPE_KEYBOARD_LAYOUT: &str = "TISTypeKeyboardLayout";

/// Уникальный ID английской раскладки US по умолчанию.
pub const SOURCE_ID_US: &str = "com.apple.keylayout.US";
/// Уникальный ID русской раскладки.
pub const SOURCE_ID_RU_PC: &str = "com.apple.keylayout.RussianPC";
pub const SOURCE_ID_RU: &str = "com.apple.keylayout.Russian";

extern "C" {
    // ---- CoreFoundation ----
    pub fn CFStringCreateWithCString(
        alloc: *const c_void,
        cStr: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
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
    if ok == 1 {
        let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const c_char);
        Some(cstr.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Создаёт CFString из Rust-строки. Caller обязан освободить через CFRelease.
///
/// # Safety
/// Результат — «unmanaged» CF-объект; утечка возможна при отсутствии CFRelease.
#[allow(dead_code)]
pub unsafe fn string_to_cfstring(s: &str) -> Option<CFStringRef> {
    let c = std::ffi::CString::new(s).ok()?;
    Some(CFStringCreateWithCString(
        core::ptr::null(),
        c.as_ptr(),
        kCFStringEncodingUTF8,
    ))
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
    let key = string_to_cfstring(K_TIS_PROPERTY_ID)?;
    let prop = TISGetInputSourceProperty(src, key);
    let id = if prop.is_null() {
        None
    } else {
        cfstring_to_string(prop as CFStringRef)
    };
    CFRelease(key);
    id
}

/// Ищет первый доступный источник ввода с заданным ID.
///
/// # Safety
/// См. [`current_input_source_id`]. Возвращаемый ref принадлежит созданному
/// списку — его нельзя освобождать отдельно от списка.
pub unsafe fn find_input_source(id: &str) -> Option<(TISInputSourceRef, CFArrayRef)> {
    let list = TISCreateInputSourceList(core::ptr::null(), 1);
    if list.is_null() {
        return None;
    }
    let count = CFArrayGetCount(list);
    for i in 0..count {
        let src = CFArrayGetValueAtIndex(list, i) as TISInputSourceRef;
        if src.is_null() {
            continue;
        }
        let key = string_to_cfstring(K_TIS_PROPERTY_ID);
        let Some(key) = key else { continue };
        let prop = TISGetInputSourceProperty(src, key);
        let sid = if prop.is_null() {
            None
        } else {
            cfstring_to_string(prop as CFStringRef)
        };
        CFRelease(key);
        if sid.as_deref() == Some(id) {
            return Some((src, list));
        }
    }
    CFRelease(list as CFTypeRef);
    None
}

/// Пробует несколько известных ID русской раскладки.
pub const RU_CANDIDATES: &[&str] = &[SOURCE_ID_RU_PC, SOURCE_ID_RU, "com.apple.keylayout.Russian"];
pub const EN_CANDIDATES: &[&str] = &[SOURCE_ID_US, "com.apple.keylayout.British"];
