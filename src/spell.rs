//! Спеллчек и подбор вариантов исправления для попапа-подсказки.
//!
//! Вызывается по завершении слова (разделитель): если слово отсутствует в
//! частотном словаре своего языка, подбираются варианты исправления:
//!
//! * [`SuggestionKind::Typo`] — словарные слова на расстоянии
//!   Дамерау-Левенштейна 1 от напечатанного (замена, вставка, удаление,
//!   транспозиция соседних символов). Кандидаты ищутся генерацией всех
//!   edit-1 вариантов слова и бинарным поиском в словаре: сотни вариантов
//!   × O(log n) — микросекунды на слово, дополнительные индексы не нужны;
//! * [`SuggestionKind::Layout`] — те же физические клавиши в другой
//!   раскладке (вид передаёт вызывающий), если такое слово словарное.
//!
//! Ранжирование — по частоте кандидата; регистр исходного слова
//! переносится на вариант. Словарные и короткие слова не помечаются.
//!
//! Приватность: слова не логируются на уровне info (только debug/trace).

use crate::predictor::LangPair;

/// Минимальная частота кандидата-исправления: хвост словаря (частоты ~десятков)
/// — в основном шум субтитров и сам является опечатками.
const MIN_TYPO_FREQ: u64 = 100;

/// Алфавиты для генерации edit-1 вариантов (совпадают с алфавитами словарей).
const EN_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz'";
const RU_ALPHABET: &str = "абвгдеёжзийклмнопрстуфхцчшщъыьэюя-";

/// Тип варианта исправления.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// Опечатка: слово исправлено редактированием одного символа.
    Typo,
    /// Набрано в чужой раскладке: конвертация тех же клавиш.
    Layout,
}

/// Один вариант исправления.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub text: String,
    pub freq: u64,
    pub kind: SuggestionKind,
}

/// Настройки спеллчека (проекция `Config`).
#[derive(Debug, Clone)]
pub struct SpellConfig {
    pub enabled: bool,
    /// Минимальная длина слова для проверки.
    pub min_chars: usize,
    /// Сколько вариантов показывать (выбор цифрами 1–9 — не больше 9).
    pub max_suggestions: usize,
}

impl Default for SpellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_chars: 4,
            max_suggestions: 5,
        }
    }
}

/// Подбирает варианты исправления слова. Пусто — слово словарное, короткое,
/// смешанное по алфавитам или кандидатов нет.
///
/// `other_view` — те же физические клавиши в другой раскладке, если известны.
pub fn suggestions(
    langs: &LangPair,
    word: &str,
    other_view: Option<&str>,
    cfg: &SpellConfig,
) -> Vec<Suggestion> {
    let lower = word.to_lowercase();
    if lower.chars().count() < cfg.min_chars.max(1) {
        return Vec::new();
    }
    let (model, other_model, alphabet) = match script_of(&lower) {
        Script::Latin => (&langs.en, &langs.ru, EN_ALPHABET),
        Script::Cyrillic => (&langs.ru, &langs.en, RU_ALPHABET),
        Script::Other => return Vec::new(),
    };
    if model.dict.freq(&lower) > 0 {
        return Vec::new(); // словарное слово — не ошибка
    }

    let mut out: Vec<Suggestion> = Vec::new();

    // Вариант «не в той раскладке»: другой вид — словарное слово.
    if let Some(other) = other_view.map(str::to_lowercase) {
        let freq = other_model.dict.freq(&other);
        if freq > 0 {
            out.push(Suggestion {
                text: apply_case(word, other),
                freq,
                kind: SuggestionKind::Layout,
            });
        }
    }

    for (variant, freq) in typo_candidates(model, &lower, alphabet) {
        out.push(Suggestion {
            text: apply_case(word, variant),
            freq,
            kind: SuggestionKind::Typo,
        });
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.freq));
    out.truncate(cfg.max_suggestions.max(1));
    out
}

/// Словарные слова на расстоянии Дамерау-Левенштейна 1 от `lower`
/// (с частотой не ниже [`MIN_TYPO_FREQ`]), без дубликатов.
fn typo_candidates(
    model: &crate::predictor::LangModel,
    lower: &str,
    alphabet: &str,
) -> Vec<(String, u64)> {
    let chars: Vec<char> = lower.chars().collect();
    let mut out: Vec<(String, u64)> = Vec::new();
    // Рабочий буфер переиспользуется между вариантами.
    let mut buf = String::with_capacity(lower.len() + 1);

    let check = |buf: &str, out: &mut Vec<(String, u64)>| {
        if out.iter().any(|(w, _)| w == buf) {
            return;
        }
        let freq = model.dict.freq(buf);
        if freq >= MIN_TYPO_FREQ {
            out.push((buf.to_string(), freq));
        }
    };

    // Удаление одного символа.
    for i in 0..chars.len() {
        write_variant(&mut buf, &chars, i, None);
        check(&buf, &mut out);
    }
    // Транспозиция соседних символов.
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] != chars[i + 1] {
            buf.clear();
            buf.extend(chars[..i].iter().copied());
            buf.push(chars[i + 1]);
            buf.push(chars[i]);
            buf.extend(chars[i + 2..].iter().copied());
            check(&buf, &mut out);
        }
    }
    // Замена символа.
    for i in 0..chars.len() {
        for c in alphabet.chars() {
            if c != chars[i] {
                write_variant(&mut buf, &chars, i, Some(c));
                check(&buf, &mut out);
            }
        }
    }
    // Вставка символа.
    for i in 0..=chars.len() {
        for c in alphabet.chars() {
            buf.clear();
            buf.extend(chars[..i].iter().copied());
            buf.push(c);
            buf.extend(chars[i..].iter().copied());
            check(&buf, &mut out);
        }
    }
    out
}

/// Записывает в `buf` слово `chars` без позиции `skip`, при `insert` —
/// с новым символом вместо пропущенной позиции.
fn write_variant(buf: &mut String, chars: &[char], skip: usize, insert: Option<char>) {
    buf.clear();
    for (i, &c) in chars.iter().enumerate() {
        if i == skip {
            if let Some(ins) = insert {
                buf.push(ins);
            }
            continue;
        }
        buf.push(c);
    }
}

/// Перенос регистра исходного слова на вариант (ПРИВЕТ / Привет / привет).
fn apply_case(template: &str, mut s: String) -> String {
    let all_upper =
        template.chars().any(char::is_uppercase) && template.chars().all(char::is_uppercase);
    if all_upper {
        s = s.to_uppercase();
    } else if template.chars().next().is_some_and(char::is_uppercase) {
        let mut it = s.chars();
        if let Some(first) = it.next() {
            let head: String = first.to_uppercase().collect();
            s = format!("{head}{}", it.as_str());
        }
    }
    s
}

/// Алфавит слова: латиница, кириллица или смешанный/иной.
enum Script {
    Latin,
    Cyrillic,
    Other,
}

fn script_of(word: &str) -> Script {
    let mut latin = false;
    let mut cyrillic = false;
    for c in word.chars() {
        if c.is_ascii_alphabetic() {
            latin = true;
        } else if is_cyrillic(c) {
            cyrillic = true;
        } else if c != '-' && c != '\'' {
            return Script::Other; // цифры и прочее — не «слово»
        }
    }
    match (latin, cyrillic) {
        (true, false) => Script::Latin,
        (false, true) => Script::Cyrillic,
        _ => Script::Other,
    }
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'А'..='я' | 'Ё' | 'ё')
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN_TSV: &str = "hello\t5000\nhelp\t3000\nhell\t800\nworld\t9000\nthe\t100000\n\
                          xylo\t50\ntemp\t4000\ntempo\t2000\n";
    const RU_TSV: &str = "привет\t4000\nприлив\t600\nмир\t7000\nкак\t500000\n";

    fn pair() -> std::sync::Arc<LangPair> {
        LangPair::from_tsv(EN_TSV, RU_TSV)
    }

    fn cfg() -> SpellConfig {
        SpellConfig::default()
    }

    fn texts(s: &[Suggestion]) -> Vec<String> {
        s.iter().map(|x| x.text.clone()).collect()
    }

    #[test]
    fn typo_candidates_found_and_ranked() {
        // «helo» -> hello (вставка l, 5000), help (замена o->p, 3000),
        // hell (удаление o, 800) — по убыванию частоты.
        let s = suggestions(&pair(), "helo", None, &cfg());
        assert_eq!(
            texts(&s),
            vec!["hello".to_string(), "help".to_string(), "hell".to_string()]
        );
    }

    #[test]
    fn valid_word_is_not_misspelled() {
        assert!(suggestions(&pair(), "hello", None, &cfg()).is_empty());
    }

    #[test]
    fn short_word_is_skipped() {
        assert!(suggestions(&pair(), "hel", None, &cfg()).is_empty());
    }

    #[test]
    fn layout_candidate_offered() {
        // «ghbdtn» вне EN-словаря, вид «привет» — словарное русское слово.
        let s = suggestions(&pair(), "ghbdtn", Some("привет"), &cfg());
        assert_eq!(texts(&s), vec!["привет".to_string()]);
        assert_eq!(s[0].kind, SuggestionKind::Layout);
    }

    #[test]
    fn layout_candidate_requires_known_word() {
        // Другой вид «рудл» не словарный — варианта-конвертации не будет
        // (остаются только опечатки).
        let s = suggestions(&pair(), "helo", Some("рудл"), &cfg());
        assert!(s.iter().all(|x| x.kind != SuggestionKind::Layout));
    }

    #[test]
    fn case_is_preserved() {
        let s = suggestions(&pair(), "Ghbdtn", Some("привет"), &cfg());
        assert_eq!(texts(&s), vec!["Привет".to_string()]);
        let s = suggestions(&pair(), "GHBDTN", Some("привет"), &cfg());
        assert_eq!(texts(&s), vec!["ПРИВЕТ".to_string()]);
    }

    #[test]
    fn mixed_script_is_skipped() {
        assert!(suggestions(&pair(), "ghbпривет", None, &cfg()).is_empty());
    }

    #[test]
    fn rare_candidates_are_filtered_out() {
        // «xylo» (freq 50) ниже порога MIN_TYPO_FREQ.
        assert!(suggestions(&pair(), "xyloz", None, &cfg()).is_empty());
    }

    #[test]
    fn max_suggestions_limits_output() {
        let c = SpellConfig {
            max_suggestions: 1,
            ..cfg()
        };
        // «helo» имеет двух кандидатов, показываем лучшего.
        let s = suggestions(&pair(), "helo", None, &c);
        assert_eq!(texts(&s), vec!["hello".to_string()]);
    }

    #[test]
    fn russian_typos_work() {
        // «привт» (удаление е) -> «привет»; «прилив» дальше (вставка и).
        let s = suggestions(&pair(), "привт", None, &cfg());
        assert_eq!(texts(&s), vec!["привет".to_string()]);
    }

    #[test]
    fn transposition_is_handled() {
        // «wrold» — перестановка соседних r/o -> «world».
        let s = suggestions(&pair(), "wrold", None, &cfg());
        assert_eq!(texts(&s), vec!["world".to_string()]);
    }
}
