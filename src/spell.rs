//! Спеллчек и подбор вариантов исправления для попапа-подсказки.
//!
//! Вызывается по завершении слова (разделитель): если слово НЕ валидно по
//! словарям своего языка (ни в частотном, ни в орфографическом слое —
//! `LangModel::valid`), подбираются варианты исправления:
//!
//! * [`SuggestionKind::Typo`] — словарные слова на расстоянии
//!   Дамерау-Левенштейна 1 от напечатанного (замена, вставка, удаление,
//!   транспозиция соседних символов). Кандидаты ищутся генерацией всех
//!   edit-1 вариантов слова и бинарным поиском в словаре: сотни вариантов
//!   × O(log n) — микросекунды на слово, дополнительные индексы не нужны;
//! * [`SuggestionKind::Layout`] — те же физические клавиши в другой
//!   раскладке (вид передаёт вызывающий), если такое слово словарное.
//!
//! Ранжирование — noisy-channel в лог-домене: `ln(частота) + бонус за тип
//! правки`. Частота — «какое слово вероятнее»; бонус за тип правки —
//! «как похоже на реальную опечатку»: соседняя клавиша, транспозиция,
//! потерянное удвоение. Регистр исходного слова переносится на вариант.
//! Словарные и короткие слова не помечаются.
//!
//! 3-граммное правдоподобие кандидата проверялось и отброшено: модель
//! строится из того же частотного списка, поэтому словарный мусор идеально
//! «обучает» собственные уникальные триграммы и различить «слово/мусор»
//! нельзя (эксперимент — тест `eval_ranking_on_real_dicts` ниже).
//!
//! Приватность: слова не логируются на уровне info (только debug/trace).

use crate::predictor::{LangPair, ORTHO_PSEUDO_FREQ};

/// Минимальная частота кандидата-исправления: хвост словаря (частоты ~десятков)
/// — в основном шум субтитров и сам является опечатками.
const MIN_TYPO_FREQ: u64 = 100;

/// Бонус: замена на физически соседнюю клавишу (QWERTY/ЙЦУКЕН).
const ADJACENT_KEY_BONUS: f64 = 1.5;
/// Бонус: транспозиция соседних букв — классическая опечатка быстрой печати.
const TRANSPOSITION_BONUS: f64 = 2.0;
/// Бонус: правка затрагивает удвоенную букву («helo»→«hello», «helllo»→«hello»).
const DOUBLE_LETTER_BONUS: f64 = 2.0;
/// Бонус варианта «не в той раскладке»: в этом приложении самая частая ошибка.
const LAYOUT_BONUS: f64 = 1.0;

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
    if model.valid(&lower) {
        return Vec::new(); // словарное слово — не ошибка
    }

    let mut scored: Vec<(f64, Suggestion)> = Vec::new();

    // Вариант «не в той раскладке»: другой вид — словарное слово.
    if let Some(other) = other_view.map(str::to_lowercase) {
        if other_model.valid(&other) {
            // Слова без частоты (только орфографический слой) ранжируем
            // условной частотой нижней границы частотного среза.
            let freq = other_model.dict.freq(&other).max(ORTHO_PSEUDO_FREQ);
            let score = candidate_score(freq, LAYOUT_BONUS);
            scored.push((
                score,
                Suggestion {
                    text: apply_case(word, other),
                    freq,
                    kind: SuggestionKind::Layout,
                },
            ));
        }
    }

    for (variant, freq, bonus) in typo_candidates(model, &lower, alphabet) {
        let score = candidate_score(freq, bonus);
        scored.push((
            score,
            Suggestion {
                text: apply_case(word, variant),
                freq,
                kind: SuggestionKind::Typo,
            },
        ));
    }

    // Скор по убыванию; при равенстве — частота, затем текст (детерминизм).
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.total_cmp(sa)
            .then(b.freq.cmp(&a.freq))
            .then(a.text.cmp(&b.text))
    });
    scored.truncate(cfg.max_suggestions.max(1));
    scored.into_iter().map(|(_, s)| s).collect()
}

/// Итоговый скор кандидата: ln(частота) + бонус типа правки.
fn candidate_score(freq: u64, edit_bonus: f64) -> f64 {
    (f64::from(u32::try_from(freq).unwrap_or(u32::MAX)) + 1.0).ln() + edit_bonus
}

/// Словарные слова на расстоянии Дамерау-Левенштейна 1 от `lower`
/// (с частотой не ниже [`MIN_TYPO_FREQ`]), без дубликатов. Третий элемент —
/// бонус «похожести на реальную опечатку» (см. константы-бонусы).
fn typo_candidates(
    model: &crate::predictor::LangModel,
    lower: &str,
    alphabet: &str,
) -> Vec<(String, u64, f64)> {
    let chars: Vec<char> = lower.chars().collect();
    let mut out: Vec<(String, u64, f64)> = Vec::new();
    // Рабочий буфер переиспользуется между вариантами.
    let mut buf = String::with_capacity(lower.len() + 1);

    let check = |buf: &str, bonus: f64, out: &mut Vec<(String, u64, f64)>| {
        if let Some(known) = out.iter_mut().find(|(w, _, _)| w == buf) {
            // Тот же вариант мог получиться другой правкой — берём лучший бонус.
            known.2 = known.2.max(bonus);
            return;
        }
        let freq = model.dict.freq(buf);
        if freq >= MIN_TYPO_FREQ {
            out.push((buf.to_string(), freq, bonus));
        }
    };

    // Удаление одного символа.
    for i in 0..chars.len() {
        let bonus = if (i > 0 && chars[i - 1] == chars[i])
            || (i + 1 < chars.len() && chars[i + 1] == chars[i])
        {
            DOUBLE_LETTER_BONUS // лишняя буква в удвоении: «helllo»→«hello»
        } else {
            0.0
        };
        write_variant(&mut buf, &chars, i, None);
        check(&buf, bonus, &mut out);
    }
    // Транспозиция соседних символов.
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] != chars[i + 1] {
            buf.clear();
            buf.extend(chars[..i].iter().copied());
            buf.push(chars[i + 1]);
            buf.push(chars[i]);
            buf.extend(chars[i + 2..].iter().copied());
            check(&buf, TRANSPOSITION_BONUS, &mut out);
        }
    }
    // Замена символа.
    for i in 0..chars.len() {
        for c in alphabet.chars() {
            if c != chars[i] {
                let bonus = if keys_adjacent(chars[i], c) {
                    ADJACENT_KEY_BONUS
                } else {
                    0.0
                };
                write_variant(&mut buf, &chars, i, Some(c));
                check(&buf, bonus, &mut out);
            }
        }
    }
    // Вставка символа.
    for i in 0..=chars.len() {
        for c in alphabet.chars() {
            let bonus = if (i > 0 && chars[i - 1] == c) || (i < chars.len() && chars[i] == c) {
                DOUBLE_LETTER_BONUS // потерянная буква в удвоении: «helo»→«hello»
            } else {
                0.0
            };
            buf.clear();
            buf.extend(chars[..i].iter().copied());
            buf.push(c);
            buf.extend(chars[i..].iter().copied());
            check(&buf, bonus, &mut out);
        }
    }
    out
}

/// Физическая позиция клавиши (ряд, колонка с учётом ступенчатого смещения
/// рядов ANSI-клавиатуры) для QWERTY и ЙЦУКЕН. `None` — клавиши нет в рядах
/// букв (`'`, `-`, `ё` и т.п.): близость считается ложной.
fn key_pos(c: char) -> Option<(usize, f64)> {
    const ROWS: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];
    const ROWS_RU: [&str; 3] = ["йцукенгшщзхъ", "фывапролджэ", "ячсмитьбю"];
    const OFFSETS: [f64; 3] = [0.0, 0.25, 0.75];
    for (row, chars) in ROWS.into_iter().chain(ROWS_RU).enumerate() {
        if let Some(col) = chars.chars().position(|k| k == c) {
            let row = row % 3;
            return Some((row, col as f64 + OFFSETS[row]));
        }
    }
    None
}

/// Соседние ли клавиши на физической клавиатуре: в одном ряду рядом или
/// в соседних рядах не дальше клавиши по горизонтали (через ряд — уже нет).
fn keys_adjacent(a: char, b: char) -> bool {
    let (Some((r1, c1)), Some((r2, c2))) = (key_pos(a), key_pos(b)) else {
        return false;
    };
    let (dr, dc) = ((r1 as i32 - r2 as i32).abs(), (c1 - c2).abs());
    dr <= 1 && dc <= 1.0
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
    if template.chars().all(char::is_uppercase) {
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

    #[test]
    fn keys_adjacent_matches_physical_keyboard() {
        // ЙЦУКЕН: соседние в ряду и по диагонали в соседних рядах.
        assert!(keys_adjacent('о', 'л'));
        assert!(keys_adjacent('ш', 'щ'));
        assert!(keys_adjacent('ы', 'в'));
        // Через ряд и далёкие в ряду — не соседи.
        assert!(!keys_adjacent('и', 'е'));
        assert!(!keys_adjacent('а', 'ф'));
        // QWERTY: те же правила.
        assert!(keys_adjacent('o', 'p'));
        assert!(keys_adjacent('q', 'a'));
        assert!(!keys_adjacent('q', 's'));
        assert!(!keys_adjacent('q', 'z'));
        // Вне буквенных рядов близости нет.
        assert!(!keys_adjacent('ё', 'о'));
        assert!(!keys_adjacent('\'', 'o'));
    }

    #[test]
    fn adjacent_substitution_outranks_more_frequent() {
        // «hwllo»: hello (w->e, соседние клавиши, freq 5000) должен обойти
        // более частотный hollo (w->o, далёкие клавиши, freq 9000).
        let en = "hello\t5000\nhollo\t9000\n";
        let ru = "мир\t7000\n";
        let langs = LangPair::from_tsv(en, ru);
        let s = suggestions(&langs, "hwllo", None, &cfg());
        assert_eq!(texts(&s), vec!["hello".to_string(), "hollo".to_string()]);
    }

    /// Оценка ранжирования на реальных встроенных словарях: генерируем по
    /// одной типовой опечатке для частых слов и меряем, как часто задуманное
    /// слово оказывается первым (и в тройке). Сравниваются чистая частота
    /// и частота + бонусы типа правки (бонусы дали +5.6 п.п. top-1 на
    /// замере 2026-10; 3-граммный член проверен там же и отброшен —
    /// модель из того же списка не отличает слово от мусора).
    /// Запуск: `cargo test --release -- --ignored eval_ranking --nocapture`.
    #[test]
    #[ignore]
    fn eval_ranking_on_real_dicts() {
        let langs = LangPair::load_embedded();
        let model = &langs.ru;
        let n = 2000;
        let mut seed = 0x5eed_u64;
        let mut rnd = move |m: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % m
        };

        // Источник: частые слова (кандидаты проходят MIN_TYPO_FREQ), длинные.
        let sources: Vec<&str> = (0..model.dict.len())
            .filter_map(|i| {
                let (w, f) = model.dict.word_at(i);
                (f >= MIN_TYPO_FREQ && w.chars().count() >= 5).then_some(w)
            })
            .take(n)
            .collect();

        let make_typo = |word: &str, rnd: &mut dyn FnMut(u64) -> u64| -> Option<String> {
            let mut chars: Vec<char> = word.chars().collect();
            let kind = rnd(10);
            match kind {
                // 0-4: замена на соседнюю клавишу.
                0..=4 => {
                    let i = rnd(chars.len() as u64) as usize;
                    let near: Vec<char> = RU_ALPHABET
                        .chars()
                        .filter(|&c| c != chars[i] && keys_adjacent(chars[i], c))
                        .collect();
                    if near.is_empty() {
                        return None; // «ё», «-» — соседних буквенных клавиш нет
                    }
                    let c = *near.get(rnd(near.len() as u64) as usize)?;
                    chars[i] = c;
                }
                // 5-6: транспозиция.
                5..=6 => {
                    let i = rnd((chars.len() - 1) as u64) as usize;
                    if chars[i] == chars[i + 1] {
                        return None;
                    }
                    chars.swap(i, i + 1);
                }
                // 7: потерянное удвоение.
                7 => {
                    let doubles: Vec<usize> = (1..chars.len())
                        .filter(|&i| chars[i] == chars[i - 1])
                        .collect();
                    if doubles.is_empty() {
                        return None;
                    }
                    let i = *doubles.get(rnd(doubles.len() as u64) as usize)?;
                    chars.remove(i);
                }
                // 8: случайная замена.
                8 => {
                    let i = rnd(chars.len() as u64) as usize;
                    let c = RU_ALPHABET
                        .chars()
                        .nth(rnd(RU_ALPHABET.len() as u64) as usize)?;
                    if c == chars[i] {
                        return None;
                    }
                    chars[i] = c;
                }
                // 9: удаление.
                _ => {
                    chars.remove(rnd(chars.len() as u64) as usize);
                }
            }
            Some(chars.into_iter().collect())
        };

        let mut samples = 0;
        let mut hits = [0usize; 2]; // top-1: [частота, +бонусы]
        let mut hits3 = [0usize; 2];
        let mut diffs: Vec<(&str, &str, Vec<String>)> = Vec::new();
        for &word in &sources {
            let Some(typo) = make_typo(word, &mut rnd) else {
                continue;
            };
            if typo == word || model.valid(&typo) {
                continue; // опечатка не распознаётся как ошибка — попапа нет
            }
            samples += 1;
            let cands = typo_candidates(model, &typo, RU_ALPHABET);
            assert!(cands.iter().any(|(v, _, _)| v == word));
            // Два скоринга: чистая частота / частота + бонусы типа правки.
            let rank = |use_bonus: bool| -> Option<usize> {
                let mut scored: Vec<(f64, &str)> = cands
                    .iter()
                    .map(|(v, f, b)| {
                        (
                            (f64::from(u32::try_from(*f).unwrap_or(u32::MAX)) + 1.0).ln()
                                + if use_bonus { *b } else { 0.0 },
                            v.as_str(),
                        )
                    })
                    .collect();
                scored.sort_by(|(sa, _), (sb, _)| sb.total_cmp(sa));
                scored.iter().position(|(_, v)| *v == word)
            };
            let ranks = [rank(false), rank(true)];
            for (i, r) in ranks.iter().enumerate() {
                if r == &Some(0) {
                    hits[i] += 1;
                }
                if matches!(r, Some(pos) if *pos < 3) {
                    hits3[i] += 1;
                }
            }
            if ranks[0] != ranks[1] && diffs.len() < 5 {
                let mut top: Vec<String> = cands.iter().map(|(v, _, _)| v.clone()).collect();
                top.sort_by(|a, b| {
                    let sc = |v: &str| model.dict.freq(v) as f64;
                    sc(b).total_cmp(&sc(a))
                });
                diffs.push((typo.leak(), word, top));
            }
        }
        println!("образцов: {samples}");
        let labels = ["частота", "+бонусы"];
        for (i, label) in labels.iter().enumerate() {
            println!(
                "top-1 {label}: {}/{} ({:.1}%), top-3: {}/{} ({:.1}%)",
                hits[i],
                samples,
                pct(hits[i], samples),
                hits3[i],
                samples,
                pct(hits3[i], samples)
            );
        }
        for (typo, word, top) in &diffs {
            println!("различие: {typo} -> {word}: {top:?}");
        }
    }

    fn pct(a: usize, b: usize) -> f64 {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    }
}
