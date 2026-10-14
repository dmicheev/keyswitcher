//! Орфографический слой словаря: быстрый точный запрос «это вообще слово?».
//!
//! Данные — бинарный `assets/lang/<lang>_ortho.bin` (готовит
//! `tools/dict/prepare.py` из Hunspell-словарей LibreOffice утилитой
//! `unmunch`): список форм языка, отсортированный по байтам UTF-8.
//! В отличие от частотного слоя (`crate::dict`) здесь нет частот — только
//! само множество валидных слов, поэтому слой компактен и не требует разбора
//! при загрузке: байты встраиваются как есть, поиск работает прямо по ним.
//!
//! Раскрытие через `unmunch` немного теряет (часть форм hunspell считает
//! верными, а в раскрытии их нет — например, императив «здравствуй»), поэтому
//! слой дополняет частотный, а не заменяет его: [`crate::predictor::LangModel`]
//! считает слово валидным, если оно есть хоть в одном из двух.
//!
//! Формат (little-endian, автор — `write_ortho_binary` в prepare.py):
//!
//! ```text
//! u32 magic "ORTO" | u32 version | u32 count | u32 stride
//! u32 offsets[ceil(count/stride)] — позиции первых записей блоков
//! записи в порядке сортировки: u8 prefix_len | u8 suffix_len | suffix
//! ```
//!
//! Каждая запись хранит только байты, отличающие её от предыдущего слова
//! (front-coding); первая запись каждого блока — всегда целиком
//! (`prefix_len = 0`). Поиск: бинарный поиск по разреженному индексу (первые
//! слова блоков адресованы напрямую), затем линейный проход по блоку из
//! `stride` записей с докатыванием общего префикса. Рабочий буфер один и
//! переиспользуется — аллокаций в горячем пути нет.

use std::cmp::Ordering;

/// Магия и текущая версия формата.
const MAGIC: &[u8; 4] = b"ORTO";
const VERSION: u32 = 1;
/// Размер заголовка без индекса: magic + version + count + stride.
const HEADER: usize = 16;

/// Орфографический набор одного языка.
///
/// Владеет только заимствованными байтами (обычно `include_bytes!`), поэтому
/// создание дёшево, а `contains` не аллоцирует сверх одного рабочего буфера.
#[derive(Debug)]
pub struct OrthoSet {
    /// Полный бинарный блоб.
    data: &'static [u8],
    /// Количество слов (`count` из заголовка).
    count: usize,
    /// Шаг разреженного индекса (`stride` из заголовка).
    stride: usize,
}

impl OrthoSet {
    /// Пустой набор (у языка нет орфографических данных).
    pub fn empty() -> Self {
        Self {
            data: &[],
            count: 0,
            stride: 1,
        }
    }

    /// Разбирает заголовок блоба. `None` — формат не распознан или данные побиты.
    pub fn new(data: &'static [u8]) -> Option<Self> {
        if data.is_empty() {
            return Some(Self::empty());
        }
        if data.len() < HEADER {
            return None;
        }
        if &data[..4] != MAGIC || u32::from_le_bytes(data[4..8].try_into().ok()?) != VERSION {
            return None;
        }
        let count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
        let stride = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
        if stride == 0 {
            return None;
        }
        if count == 0 {
            return Some(Self::empty());
        }
        let nblocks = count.div_ceil(stride);
        let entries_start = HEADER + 4 * nblocks;
        if data.len() < entries_start {
            return None;
        }
        let offset_at = |block: usize| -> Option<usize> {
            let at = HEADER + block * 4;
            Some(u32::from_le_bytes(data[at..at + 4].try_into().ok()?) as usize)
        };
        // Контроль ссылочности: точки индекса — внутри секции записей,
        // первая — строго в её начале (иначе данные побиты).
        if offset_at(0)? != entries_start || offset_at(nblocks - 1)? >= data.len() {
            return None;
        }
        Some(Self {
            data,
            count,
            stride,
        })
    }

    /// Количество слов в наборе.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Есть ли слово в наборе.
    pub fn contains(&self, word: &str) -> bool {
        if self.count == 0 {
            return false;
        }
        let target = word.as_bytes();
        let nblocks = self.count.div_ceil(self.stride);
        // Последний блок, чьё первое слово <= target.
        let mut lo = 0usize;
        let mut hi = nblocks;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block_first(mid) <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return false; // даже первое слово набора больше target
        }
        // Линейный проход по блоку `lo - 1` с докатыванием общего префикса.
        let block = lo - 1;
        let first = block * self.stride;
        let last = (first + self.stride).min(self.count);
        let mut at = self.entry_offset(block);
        let mut word: Vec<u8> = Vec::new();
        for _ in first..last {
            at = self.decode_entry(at, &mut word);
            match word.as_slice().cmp(target) {
                Ordering::Equal => return true,
                Ordering::Greater => return false,
                Ordering::Less => {}
            }
        }
        false
    }

    /// Байты первого слова блока `block`. Первая запись блока хранит слово
    /// целиком (`prefix_len = 0`), поэтому это чистое заимствование.
    fn block_first(&self, block: usize) -> &[u8] {
        let at = self.entry_offset(block);
        debug_assert_eq!(self.data[at], 0, "первая запись блока — без префикса");
        let suffix_len = self.data[at + 1] as usize;
        &self.data[at + 2..at + 2 + suffix_len]
    }

    /// Смещение первой записи блока `block` из разреженного индекса.
    fn entry_offset(&self, block: usize) -> usize {
        let at = HEADER + block * 4;
        u32::from_le_bytes(
            self.data[at..at + 4]
                .try_into()
                .expect("индекс внутри блоба"),
        ) as usize
    }

    /// Раскодирует запись по смещению `at`, дописывая общий префикс к
    /// предыдущему слову `prev`; возвращает смещение следующей записи.
    fn decode_entry(&self, at: usize, prev: &mut Vec<u8>) -> usize {
        let prefix_len = self.data[at] as usize;
        let suffix_len = self.data[at + 1] as usize;
        prev.truncate(prefix_len);
        prev.extend_from_slice(&self.data[at + 2..at + 2 + suffix_len]);
        at + 2 + suffix_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кодировщик-двойник `write_ortho_binary` из prepare.py — для тестов.
    fn encode(words: &[&str], stride: usize) -> Vec<u8> {
        let nblocks = words.len().div_ceil(stride);
        let header = HEADER + 4 * nblocks;
        let mut body: Vec<u8> = Vec::new();
        let mut offsets: Vec<u32> = Vec::new();
        let mut prev: &[u8] = b"";
        for (i, w) in words.iter().enumerate() {
            if i % stride == 0 {
                offsets.push((header + body.len()) as u32);
                prev = b""; // первая запись блока — слово целиком
            }
            let raw = w.as_bytes();
            let shared = raw.iter().zip(prev).take_while(|(a, b)| a == b).count();
            body.push(shared as u8);
            body.push((raw.len() - shared) as u8);
            body.extend_from_slice(&raw[shared..]);
            prev = raw;
        }
        let mut out = Vec::with_capacity(header + body.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(words.len() as u32).to_le_bytes());
        out.extend_from_slice(&(stride as u32).to_le_bytes());
        for off in offsets {
            out.extend_from_slice(&off.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    fn set(words: &[&str], stride: usize) -> OrthoSet {
        let blob: &'static [u8] = Box::leak(encode(words, stride).into_boxed_slice());
        OrthoSet::new(blob).expect("валидный блоб")
    }

    #[test]
    fn empty_blob_is_valid_and_yields_nothing() {
        let s = OrthoSet::new(&[]).expect("пустые данные — валидный набор");
        assert_eq!(s.len(), 0);
        assert!(!s.contains("привет"));
    }

    #[test]
    fn contains_finds_words_across_blocks() {
        // Порядок байтовый: в UTF-8 ё (U+0451) идёт после я (U+044F).
        let words = ["абв", "абвгд", "б", "бв", "в", "жук", "зубр", "ёжик"];
        let s = set(&words, 3); // блоки по 3: проход перекрывает границы блоков
        for w in words {
            assert!(s.contains(w), "должно найтись: {w}");
        }
        assert!(!s.contains("аб"));
        assert!(!s.contains("абвгде"));
        assert!(!s.contains("аа"));
        assert!(!s.contains("жукz"));
        assert!(!s.contains("ёжи"));
    }

    #[test]
    fn prefix_is_not_a_word() {
        let s = set(&["привет", "приветствие", "прививка"], 32);
        assert!(s.contains("привет"));
        assert!(!s.contains("при"));
        assert!(!s.contains("приве"));
    }

    #[test]
    fn utf8_order_matches_byte_order() {
        // ё (U+0451) байтово больше букв основного алфавита — сортировка
        // байтовая, как в prepare.py (python sorted по кодпоинтам).
        let words = ["абв", "год", "ёжик"];
        let s = set(&words, 1);
        for w in words {
            assert!(s.contains(w));
        }
        assert!(!s.contains("её"));
    }

    #[test]
    fn stride_one_works() {
        let words = ["a", "b", "c"];
        let s = set(&words, 1);
        assert!(s.contains("b"));
        assert!(!s.contains("bb"));
    }

    #[test]
    fn malformed_blobs_are_rejected() {
        assert!(OrthoSet::new(b"ORTO").is_none());
        // Верный заголовок, но индекс обрезан.
        let mut truncated = encode(&["abc", "abd"], 1);
        truncated.truncate(20);
        assert!(OrthoSet::new(Box::leak(truncated.into_boxed_slice())).is_none());
        // Чужая магия и чужая версия.
        let mut wrong_magic = encode(&["abc"], 1);
        wrong_magic[0] = b'X';
        assert!(OrthoSet::new(Box::leak(wrong_magic.into_boxed_slice())).is_none());
        let mut wrong_version = encode(&["abc"], 1);
        wrong_version[4] = 9;
        assert!(OrthoSet::new(Box::leak(wrong_version.into_boxed_slice())).is_none());
    }

    /// Реальный встроенный набор: точечные запросы (быстро).
    #[test]
    fn embedded_ru_set_answers() {
        let s = OrthoSet::new(include_bytes!("../assets/lang/ru_ortho.bin"))
            .expect("ru_ortho.bin — валидный блоб");
        assert!(s.len() > 1_000_000);
        for w in ["привет", "мама", "переосвидетельствования"] {
            assert!(s.contains(w), "должно найтись: {w}");
        }
        for w in ["ыыы", "привт", "qwerty"] {
            assert!(!s.contains(w), "не должно найтись: {w}");
        }
    }

    #[test]
    fn embedded_en_set_answers() {
        let s = OrthoSet::new(include_bytes!("../assets/lang/en_ortho.bin"))
            .expect("en_ortho.bin — валидный блоб");
        assert!(s.len() > 100_000);
        assert!(s.contains("hello"));
        assert!(!s.contains("helo"));
    }
}
