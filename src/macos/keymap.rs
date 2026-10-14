//! Runtime keymap: таблицы «код клавиши → символ» из системных раскладок
//! через `UCKeyTranslate` (HIToolbox).
//!
//! Для включённых EN/RU источников ввода берётся `UCKeyboardLayout`
//! (`kTISPropertyUnicodeKeyLayoutData`) и переводятся все коды 0..=127
//! для двух состояний (без модификаторов / с Shift). Если источник или
//! данные раскладки недоступны — соответствующий язык берётся из
//! статической таблицы [`crate::keymap::StaticKeymap`].

use crate::engine::Layout;
use crate::keymap::codes;
use crate::keymap::{KeyChars, StaticKeymap};
use crate::macos::ffi;

/// Максимум транслируемых кодов клавиш.
const MAX_KEYCODE: u8 = 127;

/// Индекс Shift-состояния модификаторов для `UCKeyTranslate` (см. `translate_layout`).
const UC_MOD_SHIFT: u32 = 0b10;

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

impl RuntimeKeymap {
    /// Обратный поиск: клавиша (+shift), печатающая `ch` в раскладке
    /// `layout`, — для посимвольного набора замены. Пробел — реальная
    /// клавиша (в символьных таблицах его нет намеренно). Линейный скан
    /// 128 кодов × 2 регистра — вызов на строку замены, не на поток печати.
    pub fn keycode_for(&self, layout: Layout, ch: char) -> Option<(u8, bool)> {
        if ch == ' ' {
            return Some((codes::SPACE, false));
        }
        for shift in [false, true] {
            for code in 0..=MAX_KEYCODE {
                let Some((en, ru)) = self.chars_for(code, shift) else {
                    continue;
                };
                let printed = match layout {
                    Layout::En => en,
                    Layout::Ru => ru,
                    Layout::Other => None,
                };
                if printed == Some(ch) {
                    return Some((code, shift));
                }
            }
        }
        None
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
        if crate::macos::system::MacSystem::classify_source(src) == layout {
            ffi::CFRetain(src as ffi::CFTypeRef);
            found = Some(src);
            break;
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
            // UCKeyTranslate ждёт индекс состояния модификаторов (младшие
            // биты: 0 — без модификаторов, 0b10 — Shift), а не Carbon-бит
            // `shiftKey` (1<<9): его младшие биты нулевые, и таблица Shift
            // заполнялась строчными буквами.
            let modifiers = if shift { UC_MOD_SHIFT } else { 0 };
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

/// Классификация источника ввода по скрипту символов, которые он реально
/// печатает: буквенные клавиши (0x00–0x2F) транслируются без модификаторов
/// через `UCKeyTranslate`; кириллица → `Ru`, латиница → `En`. Работает для
/// любых раскладок независимо от их имени (канадская, US-International,
/// Dvorak, ...). `None` — данных раскладки нет или букв не найдено
/// (вызывающий использует фолбек по ID).
///
/// # Safety
/// `src` — валидный TISInputSourceRef из включённых источников (main-поток).
pub(crate) unsafe fn script_layout(src: ffi::TISInputSourceRef) -> Option<Layout> {
    let prop = ffi::TISGetInputSourceProperty(src, ffi::kTISPropertyUnicodeKeyLayoutData);
    if prop.is_null() {
        return None;
    }
    let layout = ffi::CFDataGetBytePtr(prop as ffi::CFDataRef) as ffi::UCKeyboardLayoutRef;
    if layout.is_null() {
        return None;
    }
    let kbd_type = u32::from(ffi::LMGetKbdType());
    let mut cyrillic = 0usize;
    let mut latin = 0usize;
    for code in 0u8..=0x2F {
        let Some(c) = translate_one(layout, code, 0, kbd_type) else {
            continue;
        };
        if is_cyrillic(c) {
            cyrillic += 1;
        } else if c.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    if cyrillic == 0 && latin == 0 {
        return None;
    }
    if cyrillic > latin {
        Some(Layout::Ru)
    } else {
        Some(Layout::En)
    }
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'А'..='я' | 'Ё' | 'ё')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Таблицы, собранные один раз: TIS/Carbon не рассчитаны на
    /// конкурентные вызовы из параллельных тестовых потоков.
    fn shared() -> &'static RuntimeKeymap {
        use std::sync::OnceLock;
        static KM: OnceLock<RuntimeKeymap> = OnceLock::new();
        KM.get_or_init(RuntimeKeymap::build)
    }

    #[test]
    fn runtime_keymap_builds_or_falls_back() {
        // На машине с включёнными EN/RU источниками соберутся runtime-таблицы,
        // иначе всё останется на статике — в обоих случаях символы доступны.
        let (en, ru) = shared().chars_for(0x0C, false).expect("q/й доступны");
        assert_eq!(en, Some('q'));
        assert_eq!(ru, Some('й'));
    }

    #[test]
    fn shifted_runtime_tables_have_uppercase() {
        // Регрессия: shift-состояние передавалось Carbon-битом (младшие
        // биты = 0), таблица Shift получала строчные буквы.
        let (en, ru) = shared().chars_for(0x0C, true).expect("q/й доступны");
        assert_eq!(en, Some('Q'));
        assert_eq!(ru, Some('Й'));
    }

    #[test]
    fn keycode_for_resolves_letters_and_shift() {
        let km = shared();
        assert_eq!(km.keycode_for(Layout::En, 'q'), Some((0x0C, false)));
        assert_eq!(km.keycode_for(Layout::En, 'Q'), Some((0x0C, true)));
        assert_eq!(km.keycode_for(Layout::Ru, 'й'), Some((0x0C, false)));
        assert_eq!(km.keycode_for(Layout::Ru, 'Й'), Some((0x0C, true)));
    }

    #[test]
    fn keycode_for_space_and_unmapped() {
        let km = shared();
        // Пробел — реальная клавиша, а не символьная таблица.
        assert_eq!(km.keycode_for(Layout::Ru, ' '), Some((codes::SPACE, false)));
        // Кириллицы в латинской раскладке нет — нужен unicode-фолбек.
        assert_eq!(km.keycode_for(Layout::En, 'я'), None);
    }
}
