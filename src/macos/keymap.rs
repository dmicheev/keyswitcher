//! Runtime keymap: таблицы «код клавиши → символ» из системных раскладок
//! через `UCKeyTranslate` (HIToolbox).
//!
//! Для включённых EN/RU источников ввода берётся `UCKeyboardLayout`
//! (`kTISPropertyUnicodeKeyLayoutData`) и переводятся все коды 0..=127
//! для двух состояний (без модификаторов / с Shift). Если источник или
//! данные раскладки недоступны — соответствующий язык берётся из
//! статической таблицы [`crate::keymap::StaticKeymap`].

use crate::engine::Layout;
use crate::keymap::{KeyChars, StaticKeymap};
use crate::macos::ffi;

/// Максимум транслируемых кодов клавиш.
const MAX_KEYCODE: u8 = 127;

/// Таблицы одного языка: [код] → символ, без shift и со shift.
type LangTables = Option<[Vec<Option<char>>; 2]>;

/// Клавиатурный профиль EN/RU из системы со static-fallback.
pub struct RuntimeKeymap {
    en: LangTables,
    ru: LangTables,
}

impl RuntimeKeymap {
    /// Собирает таблицы из включённых источников ввода.
    pub fn build() -> Self {
        Self {
            en: Self::build_lang(Layout::En),
            ru: Self::build_lang(Layout::Ru),
        }
    }

    fn build_lang(layout: Layout) -> LangTables {
        // Safety: TIS-сканирование включённых источников (main-поток).
        unsafe { find_layout_source(layout) }.and_then(|src| unsafe { translate_layout(src) })
    }

    fn lookup(&self, tables: &LangTables, keycode: u8, shift: bool, static_char: char) -> char {
        if let Some(pair) = tables {
            if let Some(c) = pair[usize::from(shift)]
                .get(usize::from(keycode))
                .copied()
                .flatten()
            {
                return c;
            }
        }
        static_char
    }
}

impl KeyChars for RuntimeKeymap {
    fn chars_for(&self, keycode: u8, shift: bool) -> Option<(Option<char>, Option<char>)> {
        // Статическая таблица решает, является ли клавиша «символьной» вообще;
        // runtime-данные уточняют символы (нестандартные раскладки).
        let (en_static, ru_static) = StaticKeymap.chars_for(keycode, shift)?;
        let en = en_static.map(|c| self.lookup(&self.en, keycode, shift, c));
        let ru = ru_static.map(|c| self.lookup(&self.ru, keycode, shift, c));
        Some((en, ru))
    }
}

/// Первый включённый источник ввода заданного языка (retained, освободить
/// вызывающему через `CFRelease`).
///
/// # Safety
/// Carbon/CF-вызовы; корректно инициализированное окружение (main-поток).
unsafe fn find_layout_source(layout: Layout) -> Option<ffi::TISInputSourceRef> {
    let list = ffi::TISCreateInputSourceList(core::ptr::null(), 0);
    if list.is_null() {
        return None;
    }
    let mut found = None;
    let count = ffi::CFArrayGetCount(list);
    for i in 0..count {
        let src = ffi::CFArrayGetValueAtIndex(list, i) as ffi::TISInputSourceRef;
        if src.is_null() {
            continue;
        }
        if let Some(sid) = ffi::input_source_id(src) {
            if crate::macos::MacSystem::classify(&sid) == layout {
                ffi::CFRetain(src as ffi::CFTypeRef);
                found = Some(src);
                break;
            }
        }
    }
    ffi::CFRelease(list as ffi::CFTypeRef);
    found
}

/// Транслирует все коды клавиш по `UCKeyboardLayout` источника.
/// Источник (retained) освобождается перед выходом.
///
/// # Safety
/// `src` — валидный TISInputSourceRef из включённых источников.
unsafe fn translate_layout(src: ffi::TISInputSourceRef) -> LangTables {
    let result = (|| {
        let prop = ffi::TISGetInputSourceProperty(src, ffi::kTISPropertyUnicodeKeyLayoutData);
        if prop.is_null() {
            log::debug!("нет UnicodeKeyLayoutData у источника — static fallback");
            return None;
        }
        let layout_ptr = ffi::CFDataGetBytePtr(prop as ffi::CFDataRef) as ffi::UCKeyboardLayoutRef;
        if layout_ptr.is_null() {
            return None;
        }
        let mut tables: [Vec<Option<char>>; 2] = [
            vec![None; MAX_KEYCODE as usize + 1],
            vec![None; MAX_KEYCODE as usize + 1],
        ];
        let kbd_type = u32::from(ffi::LMGetKbdType());
        for shift in [false, true] {
            let modifiers = if shift { ffi::CARBON_SHIFT_KEY } else { 0 };
            for code in 0..=MAX_KEYCODE {
                if let Some(c) = translate_one(layout_ptr, code, modifiers, kbd_type) {
                    tables[usize::from(shift)][code as usize] = Some(c);
                }
            }
        }
        Some(tables)
    })();
    ffi::CFRelease(src as ffi::CFTypeRef);
    result
}

/// Один вызов UCKeyTranslate → символ (без dead keys).
///
/// # Safety
/// `layout` — валидный указатель на UCKeyboardLayout.
unsafe fn translate_one(
    layout: ffi::UCKeyboardLayoutRef,
    code: u8,
    modifiers: u32,
    kbd_type: u32,
) -> Option<char> {
    let mut dead_state: u32 = 0;
    let mut out = [0u16; 8];
    let mut actual: ffi::UniCharCount = 0;
    let status = ffi::UCKeyTranslate(
        layout,
        u16::from(code),
        ffi::KUC_KEY_ACTION_DOWN,
        modifiers,
        kbd_type,
        ffi::KUC_KEY_TRANSLATE_NO_DEAD_KEYS,
        &mut dead_state,
        out.len(),
        &mut actual,
        out.as_mut_ptr(),
    );
    if status != 0 || actual == 0 {
        return None;
    }
    let s: Vec<u16> = out[..actual].to_vec();
    char::from_u32(u32::from(s[0])).filter(|c| !c.is_control() && !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_keymap_builds_or_falls_back() {
        // На машине с включёнными EN/RU источниками соберутся runtime-таблицы,
        // иначе всё останется на статике — в обоих случаях символы доступны.
        let km = RuntimeKeymap::build();
        let (en, ru) = km.chars_for(0x0C, false).expect("q/й доступны");
        assert_eq!(en, Some('q'));
        assert_eq!(ru, Some('й'));
    }
}
