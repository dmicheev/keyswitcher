//! Низкоуровневые обёртки над Carbon / TIS / CoreGraphics API.
//!
//! Используется для определения и принудительной смены раскладки клавиатуры
//! (Text Input Source), захвата клавиатурных событий (CGEventTap) и
//! построения runtime-таблиц «код клавиши → символ» (UCKeyTranslate).
//! Код компилируется только под macOS.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use core::ffi::{c_char, c_int, c_void};

pub type TISInputSourceRef = *mut c_void;
pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFDataRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFIndex = isize;
pub type OSStatus = i32;
pub type Boolean = u8;

pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFRunLoopTimerRef = *mut c_void;
/// CFRunLoopTimerCallBack: (timer, context.info).
pub type CFRunLoopTimerCallBack = extern "C" fn(CFRunLoopTimerRef, *mut c_void);
pub type CFTimeInterval = f64;
pub type CFMachPortRef = *mut c_void;

/// CFRunLoopTimerContext (см. CFRunLoop.h).
#[repr(C)]
pub struct CFRunLoopTimerContext {
    pub version: CFIndex,
    pub info: *mut c_void,
    pub retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    pub release: Option<extern "C" fn(*const c_void)>,
    pub copyDescription: Option<extern "C" fn(*const c_void) -> *const c_void>,
}

pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CGEventMask = u64;
pub type CGEventTapProxy = *mut c_void;
pub type CGKeyCode = u16;
pub type CGEventFlags = u64;

/// `const struct UCKeyboardLayout *` — непрозрачный тип HIToolbox.
pub type UCKeyboardLayoutRef = *const c_void;
pub type UniChar = u16;
pub type UniCharCount = usize;
pub type OptionBits = u32;

extern "C" {
    // ---- CoreFoundation ----
    pub fn CFStringGetCStringPtr(theString: CFStringRef, encoding: u32) -> *const c_char;
    pub fn CFStringGetLength(theString: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    pub fn CFStringGetCString(
        theString: CFStringRef,
        buffer: *mut c_char,
        bufferSize: CFIndex,
        encoding: u32,
    ) -> Boolean;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef);
    pub fn CFArrayGetCount(theArray: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(theArray: CFArrayRef, idx: CFIndex) -> *const c_void;
    pub fn CFDataGetBytePtr(theData: CFDataRef) -> *const u8;

    // ---- CoreFoundation: run loop ----
    pub static kCFRunLoopCommonModes: CFStringRef;
    pub fn CFRunLoopGetMain() -> CFRunLoopRef;
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
    pub fn CFAbsoluteTimeGetCurrent() -> CFTimeInterval;
    pub fn CFRunLoopTimerCreate(
        allocator: *mut c_void,
        fireDate: CFTimeInterval,
        interval: CFTimeInterval,
        flags: u64,
        order: CFIndex,
        callout: CFRunLoopTimerCallBack,
        context: *const CFRunLoopTimerContext,
    ) -> CFRunLoopTimerRef;
    pub fn CFRunLoopTimerInvalidate(timer: CFRunLoopTimerRef);
    pub fn CFRunLoopTimerGetInterval(timer: CFRunLoopTimerRef) -> CFTimeInterval;
    pub fn CFMachPortCreateRunLoopSource(
        allocator: *mut c_void,
        port: CFMachPortRef,
        order: CFIndex,
    ) -> CFRunLoopSourceRef;

    // ---- Carbon HIToolbox / Text Input Source ----
    // Ключи свойств TIS — экспортируемые символы HIToolbox, а не строки:
    // линкуем их напрямую.
    pub static kTISPropertyInputSourceID: CFStringRef;
    pub static kTISPropertyUnicodeKeyLayoutData: CFStringRef;

    pub fn TISCopyCurrentKeyboardInputSource() -> TISInputSourceRef;
    pub fn TISGetInputSourceProperty(source: TISInputSourceRef, key: CFStringRef) -> *mut c_void;
    pub fn TISCreateInputSourceList(properties: CFDictionaryRef, allSources: Boolean)
        -> CFArrayRef;
    pub fn TISSelectInputSource(source: TISInputSourceRef) -> OSStatus;

    /// Перевод «код клавиши + модификаторы → символ» по раскладке.
    pub fn UCKeyTranslate(
        keyLayoutPtr: UCKeyboardLayoutRef,
        virtualKeyCode: u16,
        keyAction: u16,
        modifierKeyState: u32,
        keyboardType: u32,
        keyTranslateOptions: OptionBits,
        deadKeyState: *mut u32,
        maxStringLength: UniCharCount,
        actualStringLength: *mut UniCharCount,
        unicodeString: *mut UniChar,
    ) -> OSStatus;
    /// Текущий тип клавиатуры (для UCKeyTranslate).
    pub fn LMGetKbdType() -> u8;

    // ---- CoreGraphics: события клавиатуры ----
    pub fn CGEventTapCreate(
        tap: c_int,
        place: c_int,
        options: u32,
        eventMask: CGEventMask,
        callback: CGEventTapCallBack,
        userInfo: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CGEventSourceCreate(stateID: c_int) -> CGEventSourceRef;
    pub fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtualKey: CGKeyCode,
        keyDown: bool,
    ) -> CGEventRef;
    pub fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        length: usize,
        string: *const UniChar,
    );
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
    pub fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    pub fn CGEventPost(tapLocation: c_int, event: CGEventRef);
}

/// Сигнатура колбэка CGEventTap (C ABI).
pub type CGEventTapCallBack = extern "C" fn(
    proxy: CGEventTapProxy,
    eventType: u32,
    event: CGEventRef,
    userInfo: *mut c_void,
) -> CGEventRef;

// ---- Константы CGEvent (значения проверены по заголовкам SDK) ----
/// kCGEventKeyDown.
pub const KCG_EVENT_KEY_DOWN: u32 = 10;
/// kCGEventFlagsChanged.
pub const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
/// kCGEventTapDisabledByTimeout.
pub const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
/// kCGEventTapDisabledByUserInput.
pub const KCG_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
/// kCGSessionEventTap.
pub const KCG_SESSION_EVENT_TAP: c_int = 1;
/// kCGHeadInsertEventTap.
pub const KCG_HEAD_INSERT_EVENT_TAP: c_int = 0;
/// kCGEventTapOptionDefault (активный тап).
pub const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
/// kCGEventTapOptionListenOnly (пассивный тап).
pub const KCG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
/// kCGKeyboardEventKeycode.
pub const KCG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
/// kCGKeyboardEventAutorepeat.
pub const KCG_KEYBOARD_EVENT_AUTOREPEAT: u32 = 8;
/// kCGEventSourceStateID.
pub const KCG_EVENT_SOURCE_STATE_ID: u32 = 45;
/// kCGEventSourceStateCombinedSessionState — источник наших пост-событий.
pub const KCG_EVENT_SOURCE_STATE_COMBINED: c_int = 0;
/// kCGEventSourceStateHIDSystemState — состояние «аппаратной» клавиатуры:
/// настоящие нажатия имеют sourceStateID == 1, наши посты — 0.
pub const KCG_EVENT_SOURCE_STATE_HID: c_int = 1;
/// kCGEventFlagMaskShift.
pub const KCG_FLAG_SHIFT: CGEventFlags = 0x2_0000;
/// kCGEventFlagMaskControl.
pub const KCG_FLAG_CONTROL: CGEventFlags = 0x4_0000;
/// kCGEventFlagMaskAlternate.
pub const KCG_FLAG_OPTION: CGEventFlags = 0x8_0000;
/// kCGEventFlagMaskCommand.
pub const KCG_FLAG_COMMAND: CGEventFlags = 0x10_0000;
/// kCGEventFlagMaskNumericPad.
pub const KCG_FLAG_NUMERIC_PAD: CGEventFlags = 0x20_0000;
/// kCGEventFlagMaskAlphaShift (Caps Lock).
pub const KCG_FLAG_ALPHA_SHIFT: CGEventFlags = 0x1_0000;
/// Carbon shiftKey (бит 9) — НЕ индекс состояния `UCKeyTranslate`
/// (его младшие биты нулевые); для Shift используйте `UC_MOD_SHIFT = 0b10`.
#[allow(dead_code)]
pub const CARBON_SHIFT_KEY: u32 = 1 << 9;
/// Carbon alphaLock (бит 10).
#[allow(dead_code)]
pub const CARBON_ALPHA_LOCK: u32 = 1 << 10;
/// kUCKeyActionDown.
pub const KUC_KEY_ACTION_DOWN: u16 = 0;
/// kUCKeyTranslateNoDeadKeysMask.
pub const KUC_KEY_TRANSLATE_NO_DEAD_KEYS: OptionBits = 1;

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
    // Медленная дорожка: буфер по фактической длине строки (фиксированный
    // буфер молча терял бы длинные значения).
    let length = CFStringGetLength(s);
    let max = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8);
    if max <= 0 {
        return None;
    }
    let mut buf = vec![0u8; max as usize + 1];
    let ok = CFStringGetCString(
        s,
        buf.as_mut_ptr() as *mut c_char,
        buf.len() as CFIndex,
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

/// Ищет включённый источник ввода с заданным ID.
///
/// # Safety
/// Требует корректно инициализированного Carbon-окружения (main-поток с NSRunLoop).
/// Возвращаемый ref принадлежит созданному списку — его нельзя освобождать отдельно от списка.
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
