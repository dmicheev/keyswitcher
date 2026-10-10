//! Символьная 3-граммная языковая модель (fallback для слов вне словаря).
//!
//! Строится при загрузке из того же частотного словаря с весами частот.
//! Слова обрамляются маркерами `^` и `$`; вероятности триграмм сглажены
//! Лапласом, счёт ведётся в log-домене. Скор — средний log P триграммы
//! (нормировка на длину, чтобы длинные слова не штрафовались).
//!
//! Хранение компактное: отсортированные `Vec<(ключ, счёт)}` без HashMap
//! в резидентном состоянии; ключ триграммы упакован в u64 (3 × 21 бит).

/// Маркеры начала/конца слова (не встречаются в токенах словарей).
const BEGIN: char = '^';
const END: char = '$';
/// Ёмкость символа в ключе: до 21 бита на символ (влезает любой char UTF-32).
const CHAR_BITS: u64 = 21;

/// 3-граммная модель одного языка.
#[derive(Debug)]
pub struct NGramModel {
    /// Сортированные триграммы: ключ (c1,c2,c3) -> взвешенный счёт.
    trigrams: Vec<(u64, u64)>,
    /// Сортированные контексты: ключ (c1,c2) -> суммарный счёт.
    contexts: Vec<(u64, u64)>,
    /// Размер «алфавита» для сглаживания Лапласа (+2 маркера).
    vocab: u64,
}

impl NGramModel {
    /// Строит модель по итератору `(слово, частота)` — обычно сам словарь.
    pub fn build<'a, I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (&'a str, u64)>,
    {
        let mut trigrams: Vec<(u64, u64)> = Vec::new();
        let mut contexts: Vec<(u64, u64)> = Vec::new();
        let mut alphabet: Vec<u64> = vec![BEGIN as u64, END as u64];

        for (word, freq) in entries {
            if word.is_empty() || freq == 0 {
                continue;
            }
            let chars = word.chars().map(|c| c as u64).collect::<Vec<_>>();
            for &c in &chars {
                alphabet.push(c);
            }
            let mut seq = Vec::with_capacity(chars.len() + 2);
            seq.push(BEGIN as u64);
            seq.extend_from_slice(&chars);
            seq.push(END as u64);
            for w in seq.windows(3) {
                trigrams.push((pack3(w[0], w[1], w[2]), freq));
                contexts.push((pack2(w[0], w[1]), freq));
            }
        }
        alphabet.sort_unstable();
        alphabet.dedup();
        let vocab = alphabet.len() as u64;

        Self::merge_counts(&mut trigrams);
        Self::merge_counts(&mut contexts);
        Self {
            trigrams,
            contexts,
            vocab,
        }
    }

    /// Сортирует и схлопывает дубликаты ключей, суммируя счёт (in place).
    fn merge_counts(data: &mut Vec<(u64, u64)>) {
        data.sort_unstable_by_key(|&(k, _)| k);
        let mut write = 0;
        for read in 0..data.len() {
            if write > 0 && data[write - 1].0 == data[read].0 {
                data[write - 1].1 = data[write - 1].1.saturating_add(data[read].1);
            } else {
                data[write] = data[read];
                write += 1;
            }
        }
        data.truncate(write);
    }

    /// Скор слова: средний натуральный log P триграммы (выше — «правдоподобнее»).
    /// Для пустого слова — нейтральный 0.
    pub fn score(&self, word: &str) -> f64 {
        if word.is_empty() {
            return 0.0;
        }
        let mut seq = Vec::with_capacity(word.chars().count() + 2);
        seq.push(BEGIN as u64);
        seq.extend(word.chars().map(|c| c as u64));
        seq.push(END as u64);
        let alpha = 1.0_f64;
        let v = self.vocab as f64;
        seq.windows(3)
            .map(|w| {
                let tri = self.lookup(&self.trigrams, pack3(w[0], w[1], w[2])) as f64;
                let ctx = self.lookup(&self.contexts, pack2(w[0], w[1]));
                // Неизвестный контекст: считаем его счёт нулём (сглаживание тянет вниз).
                let denom = ctx as f64 + alpha * v;
                ((tri + alpha) / denom).ln()
            })
            .sum::<f64>()
            / seq.len().saturating_sub(2) as f64
    }

    /// Бинарный поиск счёта по ключу.
    fn lookup(&self, data: &[(u64, u64)], key: u64) -> u64 {
        data.binary_search_by(|(k, _)| k.cmp(&key))
            .map_or(0, |i| data[i].1)
    }
}

/// Упаковка 3 символов в ключ (каждый до 21 бита).
fn pack3(a: u64, b: u64, c: u64) -> u64 {
    debug_assert!(a < (1 << CHAR_BITS) && b < (1 << CHAR_BITS) && c < (1 << CHAR_BITS));
    a | (b << CHAR_BITS) | (c << (2 * CHAR_BITS))
}

/// Упаковка 2 символов в ключ.
fn pack2(a: u64, b: u64) -> u64 {
    debug_assert!(a < (1 << CHAR_BITS) && b < (1 << CHAR_BITS));
    a | (b << CHAR_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(pairs: &[(&str, u64)]) -> NGramModel {
        NGramModel::build(pairs.iter().copied())
    }

    #[test]
    fn plausible_word_scores_higher_than_garbage() {
        let m = model(&[
            ("мама", 100),
            ("мыла", 50),
            ("раму", 30),
            ("мало", 20),
            ("рама", 10),
        ]);
        let good = m.score("мама");
        let mid = m.score("мамо");
        let bad = m.score("щшгз");
        assert!(good > mid, "g={good} mid={mid}");
        assert!(mid > bad, "mid={mid} bad={bad}");
    }

    #[test]
    fn russian_prefers_russian_trigrams() {
        let ru = model(&[
            ("привет", 100),
            ("природа", 40),
            ("вечер", 30),
            ("ветер", 25),
            ("текст", 15),
        ]);
        // «ghbdtn» — как выглядит «привет», набранное в EN раскладке.
        assert!(ru.score("привет") > ru.score("ghbdtn"));
    }

    #[test]
    fn empty_word_is_neutral() {
        let m = model(&[("слово", 10)]);
        assert_eq!(m.score(""), 0.0);
    }

    #[test]
    fn unseen_chars_are_penalized_but_finite() {
        let m = model(&[("cat", 5)]);
        let s = m.score("zzz");
        assert!(s.is_finite());
        assert!(s < m.score("cat"));
    }
}
