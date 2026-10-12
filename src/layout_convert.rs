//! Конвертация готового текста между раскладками EN↔RU (для выделенного
//! текста, Punto-стиль).
//!
//! Таблица соответствий строится из любой реализации [`KeyChars`]: для каждой
//! клавиши (без Shift и с Shift) берётся пара символов EN/RU и добавляется
//! в обе стороны: `en→ru` и `ru→en`. Символы, отсутствующие в таблице
//! (цифры, знаки, общие для обеих раскладок), проходят насквозь.
//!
//! Направление конвертации выбирается по большинству букв текста:
//! кириллица → EN, латиница → RU.

use std::collections::HashMap;

use crate::engine::Layout;
use crate::keymap::KeyChars;

/// Конвертер текста между парой раскладок.
pub struct LayoutConverter {
    en_to_ru: HashMap<char, char>,
    ru_to_en: HashMap<char, char>,
}

impl LayoutConverter {
    /// Строит таблицы соответствий из раскладки.
    pub fn new(km: &impl KeyChars) -> Self {
        let mut en_to_ru = HashMap::new();
        let mut ru_to_en = HashMap::new();
        for code in 0u8..=u8::MAX {
            for shift in [false, true] {
                let Some((Some(en), Some(ru))) = km.chars_for(code, shift) else {
                    continue;
                };
                if en != ru {
                    en_to_ru.insert(en, ru);
                    ru_to_en.insert(ru, en);
                }
            }
        }
        Self { en_to_ru, ru_to_en }
    }

    /// Конвертирует текст в другую раскладку. Возвращает конвертированный
    /// текст и целевую раскладку (раскладка результата). `None` — конвертировать
    /// нечего: нет ни латинских, ни кириллических букв либо текст не изменился.
    pub fn convert(&self, text: &str) -> Option<(String, Layout)> {
        let (ru, en) = text.chars().fold((0usize, 0usize), |(ru, en), c| {
            (
                ru + usize::from(is_cyrillic(c)),
                en + usize::from(c.is_ascii_alphabetic()),
            )
        });
        if ru == 0 && en == 0 {
            return None;
        }
        // При равенстве выбираем ru→en: кириллица в выделении весомее.
        let (map, target) = if ru >= en {
            (&self.ru_to_en, Layout::En)
        } else {
            (&self.en_to_ru, Layout::Ru)
        };
        let converted: String = text
            .chars()
            .map(|c| map.get(&c).copied().unwrap_or(c))
            .collect();
        (converted != text).then_some((converted, target))
    }
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'А'..='я' | 'Ё' | 'ё')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::StaticKeymap;

    fn convert(text: &str) -> Option<(String, Layout)> {
        LayoutConverter::new(&StaticKeymap).convert(text)
    }

    #[test]
    fn latin_to_cyrillic() {
        // «привет», набранное в EN-раскладке.
        assert_eq!(convert("ghbdtn"), Some(("привет".into(), Layout::Ru)));
        // Сохраняется регистр (пары с Shift).
        assert_eq!(convert("Ghbdtn"), Some(("Привет".into(), Layout::Ru)));
        // «hello», набранное в RU-раскладке (текст кириллический -> цель EN).
        assert_eq!(convert("руддщ"), Some(("hello".into(), Layout::En)));
    }

    #[test]
    fn cyrillic_to_latin() {
        assert_eq!(convert("привет"), Some(("ghbdtn".into(), Layout::En)));
        assert_eq!(convert("Привет"), Some(("Ghbdtn".into(), Layout::En)));
        assert_eq!(convert("ЁЖИК"), Some(("~:BR".into(), Layout::En)));
    }

    #[test]
    fn digits_and_symbols_pass_through() {
        // Цифры одинаковы в обеих раскладках — текст не меняется.
        assert_eq!(convert("123 +="), None);
        // Цифры остаются, буквы конвертируются.
        assert_eq!(
            convert("123 ghbdtn"),
            Some(("123 привет".into(), Layout::Ru))
        );
    }

    #[test]
    fn direction_by_majority_script() {
        // Больше кириллицы — конвертируем в EN, латиница проходит насквозь.
        assert_eq!(
            convert("abc привет"),
            Some(("abc ghbdtn".into(), Layout::En))
        );
        // Больше латиницы — в RU.
        assert_eq!(
            convert("ghbdtn abcde"),
            Some(("привет фисву".into(), Layout::Ru))
        );
    }

    #[test]
    fn punctuation_follows_direction() {
        // Обратный апостроф в EN — «ё» в RU.
        assert_eq!(convert("ghbdtn`"), Some(("приветё".into(), Layout::Ru)));
        // Кавычка-«э» в RU — апостроф в EN.
        assert_eq!(convert("э"), Some(("'".into(), Layout::En)));
    }

    #[test]
    fn empty_and_unconvertible() {
        assert_eq!(convert(""), None);
        assert_eq!(convert("!!! ???"), None);
    }
}
