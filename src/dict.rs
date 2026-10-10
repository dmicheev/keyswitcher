//! Частотный словарь языка с быстрыми точным и префиксным запросами.
//!
//! Данные — TSV `слово\tчастота` (готовит `tools/dict/prepare.py` из FrequencyWords).
//! Слова хранятся отсортированным `Vec`, поэтому все слова с заданным префиксом
//! образуют непрерывный диапазон; максимум частоты по диапазону считается
//! блочной декомпозицией + sparse table (блок 64, O(1) на запрос, без линейного
//! скана даже для односимвольных префиксов, покрывающих десятки тысяч слов).

use std::collections::HashMap;

/// Размер блока блочной декомпозиции.
const BLOCK: usize = 64;

/// Частотный словарь одного языка.
#[derive(Debug)]
pub struct FrequencyDict {
    /// Слова в лексикографическом порядке (без дубликатов).
    words: Vec<Box<str>>,
    /// Частоты, параллельно `words`.
    freqs: Vec<u64>,
    /// Sparse table по максимумам блоков длины `BLOCK`:
    /// sparse[k][i] = max(freqs в блоках i .. i+2^k).
    sparse: Vec<Vec<u64>>,
}

impl FrequencyDict {
    /// Разбирает TSV-текст (`word\tcount` на строку). Пустые/битые строки
    /// пропускаются; дубликаты суммируются; результат сортируется.
    pub fn load(text: &str) -> Self {
        let mut acc: HashMap<Box<str>, u64> = HashMap::new();
        for line in text.lines() {
            let Some((word, count)) = line.split_once('\t') else {
                continue;
            };
            let Ok(count) = count.trim().parse::<u64>() else {
                continue;
            };
            if word.is_empty() {
                continue;
            }
            *acc.entry(word.into()).or_insert(0) += count;
        }
        let mut pairs: Vec<(Box<str>, u64)> = acc.into_iter().collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let words: Vec<Box<str>> = pairs.iter().map(|(w, _)| w.clone()).collect();
        let freqs: Vec<u64> = pairs.iter().map(|(_, f)| *f).collect();
        Self::from_sorted(words, freqs)
    }

    /// Собирает словарь из уже отсортированных по словам списков.
    fn from_sorted(words: Vec<Box<str>>, freqs: Vec<u64>) -> Self {
        let block_max: Vec<u64> = freqs
            .chunks(BLOCK)
            .map(|c| c.iter().copied().max().unwrap_or(0))
            .collect();
        // sparse[k][i] = max(block_max[i .. i + 2^k]) — уровни, пока 2^k <= nb.
        let nb = block_max.len();
        let mut sparse: Vec<Vec<u64>> = Vec::new();
        sparse.push(block_max.clone());
        let mut k = 1;
        while (1usize << k) <= nb {
            let half = 1usize << (k - 1);
            let width = 1usize << k;
            let prev = &sparse[k - 1];
            let cur = (0..=nb - width)
                .map(|i| prev[i].max(prev[i + half]))
                .collect();
            sparse.push(cur);
            k += 1;
        }
        Self {
            words,
            freqs,
            sparse,
        }
    }

    /// Количество слов в словаре.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Слово с частотой по индексу (для построения n-граммной модели).
    pub fn word_at(&self, i: usize) -> (&str, u64) {
        (self.words[i].as_ref(), self.freqs[i])
    }

    /// Точная частота слова (0 — слова нет).
    pub fn freq(&self, word: &str) -> u64 {
        self.words
            .binary_search_by(|w| w.as_ref().cmp(word))
            .ok()
            .map_or(0, |i| self.freqs[i])
    }

    /// Максимальная частота среди слов с данным префиксом (0 — префикса нет).
    pub fn max_freq_prefix(&self, prefix: &str) -> u64 {
        if prefix.is_empty() {
            return self.freqs.iter().copied().max().unwrap_or(0);
        }
        let lo = self.words.partition_point(|w| w.as_ref() < prefix);
        if lo >= self.words.len() {
            return 0;
        }
        // Внутри [lo..] предикат «начинается с prefix» монотонен (true, затем false).
        let in_range = lo + self.words[lo..].partition_point(|w| w.starts_with(prefix));
        if in_range == lo {
            return 0;
        }
        self.range_max(lo, in_range)
    }

    /// Максимум частоты на полуинтервале [l, r).
    fn range_max(&self, l: usize, r: usize) -> u64 {
        debug_assert!(l < r && r <= self.freqs.len());
        let bl = l / BLOCK;
        let br = (r - 1) / BLOCK;
        if bl == br {
            return self.freqs[l..r].iter().copied().max().unwrap_or(0);
        }
        let head = self.freqs[l..(bl + 1) * BLOCK]
            .iter()
            .copied()
            .max()
            .unwrap_or(0);
        let tail = self.freqs[br * BLOCK..r].iter().copied().max().unwrap_or(0);
        let mut best = head.max(tail);
        // Полные блоки [bl+1, br).
        let (fl, fr) = (bl + 1, br);
        if fl < fr {
            let len = fr - fl;
            let k = usize::BITS as usize - 1 - (len).leading_zeros() as usize;
            let table = &self.sparse[k];
            best = best.max(table[fl].max(table[fr - (1 << k)]));
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tsv(pairs: &[(&str, u64)]) -> String {
        pairs
            .iter()
            .map(|(w, c)| format!("{w}\t{c}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn load_parses_and_dedups() {
        let d = FrequencyDict::load(&tsv(&[
            ("мама", 10),
            ("мыла", 5),
            ("мама", 7), // дубликат: суммируется
            ("раму", 1),
            ("битая строка", 0), // без таба-числа -> пропущена… нет: таб есть, число есть
        ]));
        // «битая строка» содержит пробел — фильтрацией на уровне данных занимается
        // prepare.py, загрузчик принимает как есть.
        assert_eq!(d.len(), 4);
        assert_eq!(d.freq("мама"), 17);
        assert_eq!(d.freq("мыла"), 5);
        assert_eq!(d.freq("нету"), 0);
    }

    #[test]
    fn exact_and_prefix_queries() {
        let mut pairs = vec![
            ("aba", 3),
            ("abc", 100),
            ("abcd", 7),
            ("abzz", 40),
            ("b", 999),
            ("ba", 1),
        ];
        pairs.sort_unstable();
        let d = FrequencyDict::load(&tsv(&pairs));
        assert_eq!(d.freq("abc"), 100);
        assert_eq!(d.max_freq_prefix("ab"), 100); // max(3,100,7,40)
        assert_eq!(d.max_freq_prefix("abc"), 100);
        assert_eq!(d.max_freq_prefix("abcd"), 7);
        assert_eq!(d.max_freq_prefix("b"), 999);
        assert_eq!(d.max_freq_prefix("ba"), 1);
        assert_eq!(d.max_freq_prefix("z"), 0);
        assert_eq!(d.max_freq_prefix(""), 999);
    }

    #[test]
    fn prefix_max_on_large_generated_dict() {
        // Генерируем «словарь», где максимум префикса «a» спрятан в последнем блоке.
        let mut pairs: Vec<(String, u64)> = Vec::new();
        for i in 0..20_000 {
            let w = if i == 19_999 {
                format!("a{w}", w = "z".repeat(3))
            } else if i % 2 == 0 {
                format!("a{:04}", i)
            } else {
                format!("b{:04}", i)
            };
            pairs.push((w, (i % 97) as u64));
        }
        // Прячем максимум: слово с префиксом «azzz» имеет частоту-рекорд.
        pairs.push(("azzz".to_string(), 1_000_000));
        pairs.sort();
        pairs.dedup();
        let mut text = String::new();
        for (w, c) in &pairs {
            text.push_str(w);
            text.push('\t');
            text.push_str(&c.to_string());
            text.push('\n');
        }
        let d = FrequencyDict::load(&text);
        // Слово «azzz» встретилось и в генераторе (частота 17), и как рекорд —
        // загрузчик суммирует дубликаты: 1_000_000 + 17.
        assert_eq!(d.max_freq_prefix("a"), 1_000_017);
        assert_eq!(d.max_freq_prefix("azz"), 1_000_017);
        assert_eq!(d.max_freq_prefix("azzz"), 1_000_017);
        // Короткий префикс покрывает ~10k слов в разных блоках — запрос всё равно O(1).
        assert!(d.max_freq_prefix("b") > 0);
    }

    #[test]
    fn empty_dict_is_usable() {
        let d = FrequencyDict::load("");
        assert_eq!(d.len(), 0);
        assert_eq!(d.freq("x"), 0);
        assert_eq!(d.max_freq_prefix("x"), 0);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let d = FrequencyDict::load("слово\t10\nмусор\nещё\tне число\n\t5\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d.freq("слово"), 10);
    }
}
