//! Отображение «код клавиши → символ» для EN (US QWERTY) и RU (ЙЦУКЕН).
//!
//! Коды клавиш — виртуальные коды macOS (они же в `CGEventKeycode`):
//! например, Q = 0x0C = 12, ` = 0x32 = 50, Backspace = 0x33 = 51.
//! Статические таблицы — fallback (и основа unit-тестов без macOS);
//! на macOS основной путь — runtime-таблицы из `UCKeyTranslate` (см. `macos/keymap.rs`).

/// Доступ к «раскладочным» символам клавиши.
pub trait KeyChars {
    /// Пара символов для клавиши в EN и RU раскладках с учётом shift/caps.
    /// `None` — клавиша не производит символа (функциональная) либо
    /// недоступна в обеих раскладках.
    fn chars_for(&self, keycode: u8, shift: bool) -> Option<(Option<char>, Option<char>)>;
}

/// Одна клавиша: код + символы EN/RU без модификаторов и с Shift.
struct KeyDef {
    code: u8,
    en: [char; 2], // [без shift, со shift]
    ru: [char; 2],
}

/// Виртуальные коды macOS, которые нужны и вне таблиц (разделители, backspace).
#[allow(dead_code)] // часть констант нужна только тестам
pub mod codes {
    /// Enter/Return.
    pub const RETURN: u8 = 0x24;
    /// Tab.
    pub const TAB: u8 = 0x30;
    /// Space.
    pub const SPACE: u8 = 0x31;
    /// Backspace (Delete).
    pub const BACKSPACE: u8 = 0x33;
    /// Escape.
    pub const ESCAPE: u8 = 0x35;
    /// Caps Lock.
    pub const CAPS_LOCK: u8 = 0x39;
    /// F13 (дефолтная клавиша конвертации последнего слова).
    pub const F13: u8 = 0x69;
}

const fn kd(code: u8, en: [char; 2], ru: [char; 2]) -> KeyDef {
    KeyDef { code, en, ru }
}

/// Статическая таблица буквенных и знаковых клавиш US QWERTY ↔ ЙЦУКЕН.
/// Порядок: цифровой ряд, верхний ряд, средний ряд, нижний ряд.
#[rustfmt::skip]
static KEYS: &[KeyDef] = &[
    // цифровой ряд (позиции совпадают, сдвиг-наборы различаются)
    kd(0x12, ['1','!'], ['1','!']),
    kd(0x13, ['2','@'], ['2','"']),
    kd(0x14, ['3','#'], ['3','№']),
    kd(0x15, ['4','$'], ['4',';']),
    kd(0x17, ['5','%'], ['5','%']),
    kd(0x16, ['6','^'], ['6',':']),
    kd(0x1A, ['7','&'], ['7','?']),
    kd(0x1C, ['8','*'], ['8','*']),
    kd(0x19, ['9','('], ['9','(']),
    kd(0x1D, ['0',')'], ['0',')']),
    kd(0x1B, ['-','_'], ['-','_']),
    kd(0x18, ['=','+'], ['=','+']),
    // верхний ряд
    kd(0x0C, ['q','Q'], ['й','Й']),
    kd(0x0D, ['w','W'], ['ц','Ц']),
    kd(0x0E, ['e','E'], ['у','У']),
    kd(0x0F, ['r','R'], ['к','К']),
    kd(0x10, ['y','Y'], ['н','Н']),
    kd(0x11, ['t','T'], ['е','Е']),
    kd(0x20, ['u','U'], ['г','Г']),
    kd(0x22, ['i','I'], ['ш','Ш']),
    kd(0x1F, ['o','O'], ['щ','Щ']),
    kd(0x23, ['p','P'], ['з','З']),
    kd(0x21, ['[','{'], ['х','Х']),
    kd(0x1E, [']','}'], ['ъ','Ъ']),
    // средний ряд
    kd(0x00, ['a','A'], ['ф','Ф']),
    kd(0x01, ['s','S'], ['ы','Ы']),
    kd(0x02, ['d','D'], ['в','В']),
    kd(0x03, ['f','F'], ['а','А']),
    kd(0x05, ['g','G'], ['п','П']),
    kd(0x04, ['h','H'], ['р','Р']),
    kd(0x26, ['j','J'], ['о','О']),
    kd(0x28, ['k','K'], ['л','Л']),
    kd(0x25, ['l','L'], ['д','Д']),
    kd(0x29, [';',':'], ['ж','Ж']),
    kd(0x27, ['\'','"'], ['э','Э']),
    // нижний ряд
    kd(0x06, ['z','Z'], ['я','Я']),
    kd(0x07, ['x','X'], ['ч','Ч']),
    kd(0x08, ['c','C'], ['с','С']),
    kd(0x09, ['v','V'], ['м','М']),
    kd(0x0B, ['b','B'], ['и','И']),
    kd(0x2D, ['n','N'], ['т','Т']),
    kd(0x2E, ['m','M'], ['ь','Ь']),
    kd(0x2B, [',','<'], ['б','Б']),
    kd(0x2F, ['.','>'], ['ю','Ю']),
    kd(0x2C, ['/','?'], ['.',',']), // «/» в RU — точка, с Shift — запятая
    // крайние
    kd(0x32, ['`','~'], ['ё','Ё']),
    kd(0x2A, ['\\','|'], ['\\','/']),
];

/// Статическая раскладка US QWERTY / ЙЦУКЕН (fallback и тесты).
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticKeymap;

impl StaticKeymap {
    fn find(code: u8) -> Option<&'static KeyDef> {
        KEYS.iter().find(|k| k.code == code)
    }
}

impl KeyChars for StaticKeymap {
    fn chars_for(&self, keycode: u8, shift: bool) -> Option<(Option<char>, Option<char>)> {
        let k = Self::find(keycode)?;
        let i = usize::from(shift);
        Some((Some(k.en[i]), Some(k.ru[i])))
    }
}

/// Код клавиши по латинской букве (EN-вид, регистронезависимо) —
/// для парсинга горячих клавиш вида `cmd+t`.
pub fn letter_keycode(c: char) -> Option<u8> {
    KEYS.iter()
        .find(|k| k.en[0] == c || k.en[1] == c)
        .map(|k| k.code)
}

/// Строчная латинская буква по коду клавиши, если клавиша буквенная.
pub fn letter_name(code: u8) -> Option<char> {
    StaticKeymap::find(code).map(|k| k.en[0])
}

/// Поиск кода клавиши по символу в заданной раскладке (для тестов:
/// «напечатать слово» — последовательность кодов).
#[cfg(test)]
pub(crate) fn keycode_for(layout: crate::engine::Layout, c: char) -> Option<u8> {
    use crate::engine::Layout;
    let lc = c.to_lowercase().next().unwrap_or(c);
    KEYS.iter()
        .find(|k| {
            let variants = match layout {
                Layout::En => &k.en,
                Layout::Ru => &k.ru,
                Layout::Other => return false,
            };
            variants[0] == lc || variants[1] == lc
        })
        .map(|k| k.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both(code: u8) -> (char, char) {
        let Some((Some(en), Some(ru))) = StaticKeymap.chars_for(code, false) else {
            panic!("нет символа для 0x{code:02X}");
        };
        (en, ru)
    }

    #[test]
    fn qwerty_yoke_map() {
        assert_eq!(both(0x0C), ('q', 'й')); // Q = 0x0C = 12 (из промта)
        assert_eq!(both(0x00), ('a', 'ф'));
        assert_eq!(both(0x05), ('g', 'п'));
        assert_eq!(both(0x27), ('\'', 'э'));
        assert_eq!(both(0x32), ('`', 'ё'));
        assert_eq!(both(0x21), ('[', 'х'));
        assert_eq!(both(0x1E), (']', 'ъ'));
        assert_eq!(both(0x29), (';', 'ж'));
        assert_eq!(both(0x2A), ('\\', '\\'));
    }

    #[test]
    fn shift_variants() {
        let Some((Some(en), Some(ru))) = StaticKeymap.chars_for(0x0C, true) else {
            panic!("нет shift-символа");
        };
        assert_eq!((en, ru), ('Q', 'Й'));
        let Some((Some(en), Some(ru))) = StaticKeymap.chars_for(0x13, true) else {
            panic!();
        };
        assert_eq!((en, ru), ('@', '"')); // 2 -> @ в EN, " в RU
        let Some((_, Some(ru))) = StaticKeymap.chars_for(0x2C, true) else {
            panic!();
        };
        assert_eq!(ru, ','); // «/» + shift в RU — запятая
    }

    #[test]
    fn functional_keys_have_no_chars() {
        assert!(StaticKeymap.chars_for(codes::SPACE, false).is_none());
        assert!(StaticKeymap.chars_for(codes::RETURN, false).is_none());
        assert!(StaticKeymap.chars_for(codes::BACKSPACE, false).is_none());
        assert!(StaticKeymap.chars_for(codes::F13, false).is_none());
    }
}
