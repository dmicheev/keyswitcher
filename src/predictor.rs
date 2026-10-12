//! Стейт-машина «слова по ходу печати»: скоринг двух гипотез и решение
//! об автопереключении раскладки (Punto-стиль).
//!
//! [`Predictor`] принимает поток физических клавиш (код + shift + текущая
//! раскладка) и после каждого символа, начиная с `min_prefix`, сравнивает:
//!
//! * `log(max_freq_prefix(w))` — если гипотеза слова покрыта словарём
//!   (плюс бонус за точное совпадение);
//! * иначе `α · ngram_score(w)` — 3-граммная fallback-модель;
//!
//! при перевесе «чужой» раскладки на `log(threshold)` и более возвращает
//! [`Decision::SwitchAndFix`] — переключить раскладку и заменить уже
//! набранный префикс. Гистерезис: максимум одно автопереключение на слово.
//!
//! Словом считается последовательность клавиш, дающих букву (или дефис/
//! апостроф) хоть в одной раскладке — так RU-слова с х/ж/б/ю/ъ/ё (клавиши
//! `[ ; , . ]` и backtick в EN) исправляются и из EN-раскладки.
//!
//! Приватность: содержимое буфера не логируется на уровне info (только debug/trace).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::dict::FrequencyDict;
use crate::engine::Layout;
use crate::keymap::KeyChars;
use crate::ngram::NGramModel;

/// Словарь + n-граммная модель одного языка.
pub struct LangModel {
    pub dict: FrequencyDict,
    pub ngram: NGramModel,
}

impl LangModel {
    pub fn from_tsv(text: &str) -> Self {
        let dict = FrequencyDict::load(text);
        let ngram = NGramModel::build(dict_words(&dict));
        Self { dict, ngram }
    }
}

fn dict_words(dict: &FrequencyDict) -> impl Iterator<Item = (&str, u64)> {
    // Индексация безопасна: words/freqs параллельны и неизменны после загрузки.
    (0..dict.len()).map(|i| dict.word_at(i))
}

/// Пара языковых моделей (v1: EN + RU). Данные отделены от логики —
/// обобщение на другие пары сводится к новому экземпляру.
pub struct LangPair {
    pub en: LangModel,
    pub ru: LangModel,
}

impl LangPair {
    /// Модель по раскладке.
    fn model(&self, layout: Layout) -> Option<&LangModel> {
        match layout {
            Layout::En => Some(&self.en),
            Layout::Ru => Some(&self.ru),
            Layout::Other => None,
        }
    }

    /// Загружает встроенные словари (`assets/lang/*.tsv`, `include_bytes!`).
    /// Разбор ~2×200k строк + построение n-грамм; результат кэшируется.
    pub fn load_embedded() -> Arc<Self> {
        static CACHE: OnceLock<Arc<LangPair>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let started = Instant::now();
                let en = LangModel::from_tsv(
                    std::str::from_utf8(include_bytes!("../assets/lang/en.tsv"))
                        .expect("en.tsv — валидный UTF-8"),
                );
                let ru = LangModel::from_tsv(
                    std::str::from_utf8(include_bytes!("../assets/lang/ru.tsv"))
                        .expect("ru.tsv — валидный UTF-8"),
                );
                log::info!(
                    "Словари загружены: en={} слов, ru={} слов, {:?}",
                    en.dict.len(),
                    ru.dict.len(),
                    started.elapsed()
                );
                Arc::new(Self { en, ru })
            })
            .clone()
    }

    /// Тестовый конструктор из TSV-строк (без файлов и сети).
    #[cfg(test)]
    pub fn from_tsv(en: &str, ru: &str) -> Arc<Self> {
        Arc::new(Self {
            en: LangModel::from_tsv(en),
            ru: LangModel::from_tsv(ru),
        })
    }
}

/// Настройки предиктора (проекция `Config`).
#[derive(Debug, Clone)]
pub struct PredictorConfig {
    /// Минимальная длина префикса для начала скоринга.
    pub min_prefix: usize,
    /// Порог переключения «во сколько раз» (score-разница сравнивается с ln от него).
    pub switch_threshold: f64,
    /// Вес n-граммного счёта α.
    pub ngram_weight: f64,
}

impl Default for PredictorConfig {
    fn default() -> Self {
        Self {
            min_prefix: 3,
            switch_threshold: 100.0,
            ngram_weight: 3.0,
        }
    }
}

/// Бонус (в натах) за полное совпадение слова со словарём.
const EXACT_BONUS: f64 = 1.0;
/// Пауза, после которой слово считается оборванным.
pub const WORD_PAUSE: Duration = Duration::from_secs(3);
/// Максимум символов в слове — дальше не решаем (защита от «залипания»).
const MAX_WORD: usize = 64;

/// Решение по текущему слову.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Ничего не делать.
    None,
    /// Переключиться на `layout` и заменить набранный префикс на `replacement`.
    SwitchAndFix {
        layout: Layout,
        replacement: String,
        /// Сколько символов уже набрано (backspace × k для ретро-фикса).
        typed: usize,
    },
}

/// Один нажатый символ слова: код клавиши и её вид в обеих раскладках.
#[derive(Debug, Clone, Copy)]
struct KeyPress {
    #[allow(dead_code)]
    code: u8,
    en: Option<char>,
    ru: Option<char>,
}

/// Результат конвертации для хоткея: сколько backspace нажать и что ввести.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversion {
    pub backspaces: usize,
    pub text: String,
}

/// Слово, зафиксированное по разделителю (для спеллчек-подсказок).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedWord {
    /// Слово в EN-раскладке (`None`, если не все клавиши производят
    /// символ в ней; пунктуация «чужой» раскладки допускается).
    pub view_en: Option<String>,
    /// Слово в RU-раскладке.
    pub view_ru: Option<String>,
    /// Сколько символов набрано (backspace × k для исправления).
    pub typed: usize,
    /// Разделитель-клавиша (space/enter/tab), завершившая слово;
    /// `None` — слово оборвано цифрой/пунктуацией/паузой.
    pub sep: Option<char>,
    /// Слово уже исправлено автопереключением — подсказки не нужны.
    pub auto_switched: bool,
}

/// Стейт-машина текущего слова.
pub struct Predictor {
    langs: Arc<LangPair>,
    cfg: PredictorConfig,
    /// Символы текущего (незавершённого) слова.
    buffer: Vec<KeyPress>,
    /// Последнее зафиксированное слово (для конвертации по хоткею).
    last_word: Option<Vec<KeyPress>>,
    /// Символ, идущий в тексте сразу после последнего зафиксированного слова:
    /// разделитель (space/enter/tab) либо цифра/пунктуация, оборвавшие слово.
    /// `None` — слово оборвано паузой/функциональной клавишей (контекст у
    /// курсора неизвестен — конвертация без «лишнего» backspace).
    last_after: Option<char>,
    /// Зафиксированное слово, ожидающее обработки спеллчеком.
    pending: Option<CommittedWord>,
    /// Сработало ли автопереключение в текущем слове (гистерезис).
    switched: bool,
    last_event: Option<Instant>,
}

impl Predictor {
    pub fn new(langs: Arc<LangPair>, cfg: PredictorConfig) -> Self {
        Self {
            langs,
            cfg,
            buffer: Vec::new(),
            last_word: None,
            last_after: None,
            pending: None,
            switched: false,
            last_event: None,
        }
    }

    /// Языковые модели (для спеллчека и других потребителей).
    pub fn langs(&self) -> &Arc<LangPair> {
        &self.langs
    }

    /// Обновляет настройки (горячая перезагрузка конфига).
    pub fn update_config(&mut self, cfg: PredictorConfig) {
        self.cfg = cfg;
    }

    /// Забирает зафиксированное слово для спеллчека (одноразово).
    pub fn take_committed(&mut self) -> Option<CommittedWord> {
        self.pending.take()
    }

    /// Сбрасывает текущее слово (смена приложения, модификаторы, пауза...).
    pub fn reset_word(&mut self) {
        self.commit();
    }

    /// Полный сброс, включая последнее слово (хоткей-конвертация выполнена).
    pub fn forget_last(&mut self) {
        self.buffer.clear();
        self.last_word = None;
        self.last_after = None;
        self.pending = None;
        self.switched = false;
    }

    /// Обрабатывает нажатие клавиши. Возвращает решение, если пора
    /// переключаться. Разделители/цифры/символы завершают слово.
    pub fn feed_key(
        &mut self,
        keycode: u8,
        shift: bool,
        current_layout: Layout,
        layout_changed_by_user: bool,
        keymap: &dyn KeyChars,
        now: Instant,
    ) -> Decision {
        if layout_changed_by_user {
            // Пользователь сам выбрал раскладку — доверяем ему, слово обрываем.
            self.reset_word();
            self.last_event = Some(now);
            return Decision::None;
        }
        // Пауза: слово считается оборванным, начинаем новое.
        if let Some(t) = self.last_event {
            if now.duration_since(t) > WORD_PAUSE {
                self.reset_word();
            }
        }
        self.last_event = Some(now);

        let Some((en, ru)) = keymap.chars_for(keycode, shift) else {
            // Функциональная клавиша — граница слова. Backspace — правка
            // набранного: «последнее слово» больше не гарантированно у
            // курсора, забываем его целиком. Стрелки и прочие клавиши,
            // двигающие курсор, — то же самое. Реальные разделители
            // (space/enter/tab) отмечаем для хоткея и спеллчек-подсказок.
            if keycode == crate::keymap::codes::BACKSPACE {
                self.forget_last();
                return Decision::None;
            }
            let sep = is_separator_key(keycode).then(|| sep_char(keycode));
            self.commit_after(sep);
            if sep.is_none() {
                // Курсор мог уйти — конвертировать «последнее слово» небезопасно.
                self.last_word = None;
            }
            return Decision::None;
        };
        let (current_char, other_char) = match current_layout {
            Layout::En => (en, ru),
            Layout::Ru => (ru, en),
            Layout::Other => {
                self.commit();
                return Decision::None;
            }
        };
        let Some(current_char) = current_char else {
            // Клавиша не производит символа в текущей раскладке.
            self.commit();
            return Decision::None;
        };

        // Словная клавиша: буква/дефис/апостроф в текущей раскладке ИЛИ в
        // другой. Второе — клавиши вроде `[` (в RU — «х») или `,` (в RU —
        // «б»): без них RU-слова с х/ж/б/ю/ъ/ё, набранные в EN-раскладке,
        // обрывались бы на первой же такой букве и не исправлялись.
        // Пунктуация в гипотезе чужой раскладки топит её скоринг — ложных
        // переключений порог switch_threshold не пускает.
        let word_key = is_word_char(current_char) || other_char.is_some_and(is_word_char);
        if !word_key {
            // Цифра/пунктуация в обеих раскладках — слово закончилось.
            // Запоминаем сам символ: хоткей-конвертация сотрёт и введёт
            // его обратно, не подменяя пробелом.
            self.commit_after(Some(current_char));
            return Decision::None;
        }

        if self.buffer.len() >= MAX_WORD {
            self.commit();
        }
        self.buffer.push(KeyPress {
            code: keycode,
            en,
            ru,
        });
        self.decide(current_layout)
    }

    /// Скоринг и решение по текущему буферу.
    fn decide(&mut self, current_layout: Layout) -> Decision {
        if self.switched || self.buffer.len() < self.cfg.min_prefix {
            return Decision::None;
        }
        let Some(en_word) = self.view(Layout::En) else {
            return Decision::None;
        };
        let Some(ru_word) = self.view(Layout::Ru) else {
            return Decision::None;
        };
        let en_lower = en_word.to_lowercase();
        let ru_lower = ru_word.to_lowercase();

        let s_en = self.score(Layout::En, &en_lower);
        let s_ru = self.score(Layout::Ru, &ru_lower);
        if !s_en.is_finite() && !s_ru.is_finite() {
            return Decision::None;
        }

        let (best_layout, best_word, best_score) = if s_ru > s_en {
            (Layout::Ru, ru_word, s_ru)
        } else {
            (Layout::En, en_word, s_en)
        };
        if best_layout == current_layout {
            return Decision::None;
        }
        let current_score = self.score(
            current_layout,
            best_word_of(current_layout, &en_lower, &ru_lower),
        );
        let margin = best_score - current_score;
        if margin >= self.cfg.switch_threshold.max(1.0).ln() {
            self.switched = true;
            log::debug!(
                "переключение {:?} -> {:?}: margin={:.2}",
                current_layout,
                best_layout,
                margin
            );
            return Decision::SwitchAndFix {
                layout: best_layout,
                replacement: best_word,
                typed: self.buffer.len(),
            };
        }
        Decision::None
    }

    fn score(&self, layout: Layout, lowercase_word: &str) -> f64 {
        let Some(model) = self.langs.model(layout) else {
            return f64::NEG_INFINITY;
        };
        let max_prefix = model.dict.max_freq_prefix(lowercase_word);
        if max_prefix > 0 {
            let mut s = (max_prefix as f64).ln();
            if model.dict.freq(lowercase_word) > 0 {
                s += EXACT_BONUS;
            }
            s
        } else {
            self.cfg.ngram_weight * model.ngram.score(lowercase_word)
        }
    }

    /// Слово в заданной раскладке (с сохранением регистра) — `None`, если
    /// хоть одна клавиша не отображается в буквенный символ этой раскладки.
    fn view(&self, layout: Layout) -> Option<String> {
        view_of(&self.buffer, layout)
    }

    /// Фиксирует текущее слово как «последнее» (для хоткея) и очищает буфер.
    fn commit(&mut self) {
        self.commit_after(None);
    }

    /// Фиксирует слово; `after` — символ, идущий в тексте сразу за словом
    /// (разделитель либо цифра/пунктуация, оборвавшая слово; `None` —
    /// неизвестно: пауза, функциональная клавиша).
    fn commit_after(&mut self, after: Option<char>) {
        if !self.buffer.is_empty() {
            let word = std::mem::take(&mut self.buffer);
            // Спеллчек-подсказки показываем только по «настоящему»
            // разделителю (space/enter/tab).
            let sep = after.filter(|&c| matches!(c, ' ' | '\n' | '\t'));
            self.last_after = after;
            self.pending = Some(CommittedWord {
                view_en: view_of(&word, Layout::En),
                view_ru: view_of(&word, Layout::Ru),
                typed: word.len(),
                sep,
                auto_switched: self.switched,
            });
            self.last_word = Some(word);
        }
        self.switched = false;
    }

    /// Конвертация последнего слова в другую раскладку (хоткей).
    /// Берёт текущее недонабранное слово, иначе последнее зафиксированное
    /// (восстанавливая символ-разделитель после него). `None` — нечего
    /// конвертировать.
    pub fn convert_last_word(&self, current_layout: Layout) -> Option<Conversion> {
        let (word, after) = if !self.buffer.is_empty() {
            (self.buffer.as_slice(), None)
        } else {
            (self.last_word.as_ref()?.as_slice(), self.last_after)
        };
        let target = match current_layout {
            Layout::En => Layout::Ru,
            _ => Layout::En,
        };
        let mut text = String::with_capacity(word.len() + 1);
        for kp in word {
            let c = match target {
                Layout::En => kp.en,
                Layout::Ru => kp.ru,
                Layout::Other => None,
            }?;
            if !is_word_char(c) {
                return None;
            }
            text.push(c);
        }
        let mut backspaces = word.len();
        if let Some(c) = after {
            backspaces += 1; // символ после слова тоже стираем и вернём
            text.push(c);
        }
        (!text.is_empty()).then_some(Conversion { backspaces, text })
    }
}

fn best_word_of<'a>(layout: Layout, en_lower: &'a str, ru_lower: &'a str) -> &'a str {
    match layout {
        Layout::En => en_lower,
        _ => ru_lower,
    }
}

/// Буква либо внутрисловный дефис/апостроф (алфавиты словарей: en `[a-z']`, ru `[а-яё-]`).
fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c == '-' || c == '\''
}

/// Разделитель по коду клавиши (для восстановления при хоткей-конвертации).
fn sep_char(keycode: u8) -> char {
    match keycode {
        crate::keymap::codes::RETURN => '\n',
        crate::keymap::codes::TAB => '\t',
        _ => ' ',
    }
}

/// Клавиша-разделитель, завершающая слово «настоящим» пробелом/enter/tab.
fn is_separator_key(keycode: u8) -> bool {
    matches!(
        keycode,
        crate::keymap::codes::SPACE | crate::keymap::codes::RETURN | crate::keymap::codes::TAB
    )
}

/// Слово из нажатых клавиш в заданной раскладке (`None`, если хоть одна
/// клавиша не отображается в символ этой раскладки). Пунктуация чужой
/// раскладки («[» при RU-гипотезе «х») допускается: словари и n-граммы
/// оценивают такую гипотезу как мусор сами.
fn view_of(word: &[KeyPress], layout: Layout) -> Option<String> {
    let mut out = String::with_capacity(word.len());
    for kp in word {
        let (c, other) = match layout {
            Layout::En => (kp.en, kp.ru),
            Layout::Ru => (kp.ru, kp.en),
            Layout::Other => return None,
        };
        let c = c?;
        if !(is_word_char(c) || other.is_some_and(is_word_char)) {
            return None;
        }
        out.push(c);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{keycode_for, StaticKeymap};

    const EN_TSV: &str = "\
        the\t10000000\nand\t8000000\nto\t7000000\nyou\t4000000\nget\t5000\ngirl\t1000\ngo\t20000\n";
    const RU_TSV: &str = "\
        как\t1000\nпривет\t500\nпри\t100\nпример\t400\nправда\t300\nвагон\t300\nприват\t250\n\
        математика\t200\nтелевизор\t100\nлето\t250\nулей\t150\nтема\t90\n\
        планета\t300\nтакси\t400\nмрак\t500\nиврит\t200\nсхема\t5000\n";

    fn pair() -> Arc<LangPair> {
        LangPair::from_tsv(EN_TSV, RU_TSV)
    }

    fn pred(cfg: PredictorConfig) -> Predictor {
        Predictor::new(pair(), cfg)
    }

    fn t0() -> Instant {
        Instant::now()
    }

    /// Печатает слово в указанной раскладке, по одной клавише, шаг `step` мс.
    fn type_word(
        p: &mut Predictor,
        word: &str,
        layout: Layout,
        now: &mut Instant,
        step: Duration,
    ) -> Vec<Decision> {
        let km = StaticKeymap;
        let mut out = Vec::new();
        for c in word.chars() {
            let code = keycode_for(layout, c).unwrap_or_else(|| panic!("нет клавиши для '{c}'"));
            *now += step;
            out.push(p.feed_key(code, c.is_uppercase(), layout, false, &km, *now));
        }
        out
    }

    fn switch_only(d: &[Decision]) -> Vec<Decision> {
        d.iter()
            .filter(|d| !matches!(d, Decision::None))
            .cloned()
            .collect()
    }

    #[test]
    fn ghbdtn_in_en_switches_to_ru_on_third_char() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        let ds = type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        // Решение ровно одно, на 3-м символе.
        assert_eq!(
            ds[2],
            Decision::SwitchAndFix {
                layout: Layout::Ru,
                replacement: "при".into(),
                typed: 3,
            }
        );
        // Гистерезис: после переключения решений до конца слова нет...
        assert!(ds[3..].iter().all(|d| matches!(d, Decision::None)));
        // ...и после разделителя флаг сбрасывается.
        now += Duration::from_millis(50);
        assert_eq!(
            p.feed_key(
                crate::keymap::codes::SPACE,
                false,
                Layout::Ru,
                false,
                &StaticKeymap,
                now
            ),
            Decision::None
        );
        // Те же физические клавиши, что «ghbdtn», но слово русское в RU-раскладке.
        let ds2 = type_word(
            &mut p,
            "привет",
            Layout::Ru,
            &mut now,
            Duration::from_millis(50),
        );
        assert!(
            switch_only(&ds2).is_empty(),
            "в RU-раскладке русское слово не трогаем"
        );
    }

    #[test]
    fn russian_word_in_ru_layout_no_decisions() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        let ds = type_word(
            &mut p,
            "привет",
            Layout::Ru,
            &mut now,
            Duration::from_millis(50),
        );
        assert!(switch_only(&ds).is_empty());
    }

    #[test]
    fn collision_switches_only_with_confident_margin() {
        // «rfr» (вне en-словаря) vs «как» (ru, частое): огромный отрыв -> переключаем.
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        let ds = type_word(
            &mut p,
            "rfr",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert_eq!(
            switch_only(&ds),
            vec![Decision::SwitchAndFix {
                layout: Layout::Ru,
                replacement: "как".into(),
                typed: 3,
            }]
        );

        // Оба слова в словарях с близкими частотами: ln(1.01) << ln(100) — не трогаем.
        let en_close = EN_TSV.to_string() + "rfr\t990000\n";
        let mut p2 = Predictor::new(
            LangPair::from_tsv(&en_close, RU_TSV),
            PredictorConfig::default(),
        );
        let mut now2 = t0();
        let ds2 = type_word(
            &mut p2,
            "rfr",
            Layout::En,
            &mut now2,
            Duration::from_millis(50),
        );
        assert!(
            switch_only(&ds2).is_empty(),
            "близкие частоты — не переключаем"
        );
    }

    #[test]
    fn oov_word_with_ngram_skew_switches() {
        // «bdtnfr» — вне обоих словарей (нет префиксов «bdt»/«иве»), но ru-вид
        // «иветак» состоит из известных 3-грамм (привет/планета/такси/мрак/иврит),
        // а en-вид — сплошной мусор. Явный n-граммный перекос -> переключение.
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        let ds = type_word(
            &mut p,
            "bdtnfr",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert_eq!(
            switch_only(&ds),
            vec![Decision::SwitchAndFix {
                layout: Layout::Ru,
                replacement: "ивет".into(),
                typed: 4,
            }]
        );
    }

    #[test]
    fn digit_breaks_word_and_resets() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        // «ghb» в EN -> переключение на RU на 3-м символе...
        let ds = type_word(
            &mut p,
            "ghb",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert!(matches!(ds[2], Decision::SwitchAndFix { .. }));
        // ...затем цифра: слово обрывается, новое «dtnk» набрано уже в RU.
        now += Duration::from_millis(50);
        p.feed_key(0x12, false, Layout::Ru, false, &StaticKeymap, now); // '1'
        let mut decisions = Vec::new();
        for c in "dtnk".chars() {
            // физические клавиши ищем по EN-виду, печатаем уже в RU-раскладке
            let code = keycode_for(Layout::En, c).expect("код клавиши");
            now += Duration::from_millis(50);
            decisions.push(p.feed_key(code, false, Layout::Ru, false, &StaticKeymap, now));
        }
        assert!(
            decisions.iter().all(|d| matches!(d, Decision::None)),
            "после сброса короткое слово не даёт решений"
        );
    }

    #[test]
    fn manual_layout_change_resets_trust() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        type_word(
            &mut p,
            "gh",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        // Пользователь вручную сменил раскладку — доверие сброшено, слово обрывается.
        now += Duration::from_millis(50);
        let d = p.feed_key(0x0C, false, Layout::Ru, true, &StaticKeymap, now);
        assert_eq!(d, Decision::None);
        // Буфер сброшен: смена раскладки обрывает слово раньше, чем «q»
        // попадёт в буфер.
        assert!(p.buffer.is_empty());
    }

    #[test]
    fn pause_resets_word() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        type_word(
            &mut p,
            "gh",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        // Пауза > WORD_PAUSE: слово оборвано, «b» начинается заново.
        now += WORD_PAUSE + Duration::from_millis(100);
        let d = p.feed_key(0x0B, false, Layout::En, false, &StaticKeymap, now);
        assert_eq!(d, Decision::None); // буфер из 1 символа < min_prefix
    }

    fn no_switch_cfg() -> PredictorConfig {
        PredictorConfig {
            min_prefix: 99,
            ..PredictorConfig::default()
        }
    }

    #[test]
    fn convert_last_word_both_directions() {
        let mut p = pred(no_switch_cfg());
        let mut now = t0();
        type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert_eq!(
            p.convert_last_word(Layout::En),
            Some(Conversion {
                backspaces: 6,
                text: "привет".into()
            })
        );

        let mut p2 = pred(no_switch_cfg());
        let mut now2 = t0();
        type_word(
            &mut p2,
            "здравствуйте",
            Layout::Ru,
            &mut now2,
            Duration::from_millis(50),
        );
        assert_eq!(
            p2.convert_last_word(Layout::Ru),
            Some(Conversion {
                backspaces: 12,
                text: "plhfdcndeqnt".into()
            })
        );
    }

    #[test]
    fn convert_last_word_after_separator_restores_sep() {
        let mut p = pred(no_switch_cfg());
        let mut now = t0();
        type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        now += Duration::from_millis(50);
        p.feed_key(
            crate::keymap::codes::SPACE,
            false,
            Layout::En,
            false,
            &StaticKeymap,
            now,
        );
        assert_eq!(
            p.convert_last_word(Layout::En),
            Some(Conversion {
                backspaces: 7,
                text: "привет ".into()
            })
        );
    }

    #[test]
    fn nothing_to_convert() {
        let p = pred(PredictorConfig::default());
        assert_eq!(p.convert_last_word(Layout::En), None);
    }

    #[test]
    fn committed_word_after_separator() {
        let mut p = pred(no_switch_cfg());
        let mut now = t0();
        type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        now += Duration::from_millis(50);
        p.feed_key(
            crate::keymap::codes::SPACE,
            false,
            Layout::En,
            false,
            &StaticKeymap,
            now,
        );
        let c = p.take_committed().expect("слово зафиксировано");
        assert_eq!(c.view_en.as_deref(), Some("ghbdtn"));
        assert_eq!(c.view_ru.as_deref(), Some("привет"));
        assert_eq!(c.typed, 6);
        assert_eq!(c.sep, Some(' '));
        assert!(!c.auto_switched);
        assert!(p.take_committed().is_none(), "одноразово");
    }

    #[test]
    fn committed_word_marks_autoswitch() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        // «ghbdtn» в EN -> автопереключение на RU, затем пробел.
        type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        now += Duration::from_millis(50);
        p.feed_key(
            crate::keymap::codes::SPACE,
            false,
            Layout::Ru,
            false,
            &StaticKeymap,
            now,
        );
        let c = p.take_committed().expect("слово зафиксировано");
        assert!(c.auto_switched, "слово уже исправлено автопереключением");
    }

    #[test]
    fn committed_word_digit_is_not_separator() {
        let mut p = pred(no_switch_cfg());
        let mut now = t0();
        type_word(
            &mut p,
            "ghb",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        now += Duration::from_millis(50);
        p.feed_key(0x12, false, Layout::En, false, &StaticKeymap, now); // '1'
        let c = p.take_committed().expect("слово зафиксировано цифрой");
        assert_eq!(c.sep, None, "цифра — не разделитель для подсказок");
        // Хоткей-конвертация стирает «ghb1» и вводит «при1»: цифра
        // сохраняется, а не подменяется пробелом.
        assert_eq!(
            p.convert_last_word(Layout::En),
            Some(Conversion {
                backspaces: 4,
                text: "при1".into()
            })
        );
    }

    #[test]
    fn backspace_and_cursor_keys_invalidate_last_word() {
        // Backspace — правка набранного: «последнее слово» больше не
        // гарантированно у курсора.
        let mut p = pred(no_switch_cfg());
        let mut now = t0();
        type_word(
            &mut p,
            "ghbdtn",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        now += Duration::from_millis(50);
        p.feed_key(
            crate::keymap::codes::SPACE,
            false,
            Layout::En,
            false,
            &StaticKeymap,
            now,
        );
        now += Duration::from_millis(50);
        p.feed_key(
            crate::keymap::codes::BACKSPACE,
            false,
            Layout::En,
            false,
            &StaticKeymap,
            now,
        );
        assert_eq!(p.convert_last_word(Layout::En), None);

        // Стрелки и прочие функциональные клавиши двигают курсор — то же.
        let mut p2 = pred(no_switch_cfg());
        let mut now2 = t0();
        type_word(
            &mut p2,
            "ghbdtn",
            Layout::En,
            &mut now2,
            Duration::from_millis(50),
        );
        now2 += Duration::from_millis(50);
        p2.feed_key(
            crate::keymap::codes::SPACE,
            false,
            Layout::En,
            false,
            &StaticKeymap,
            now2,
        );
        now2 += Duration::from_millis(50);
        p2.feed_key(0x7A, false, Layout::En, false, &StaticKeymap, now2); // F1
        assert_eq!(p2.convert_last_word(Layout::En), None);
    }

    #[test]
    fn russian_word_with_punct_keys_switches_to_ru() {
        // «схема» содержит «х» — клавишу, которая в EN-раскладке даёт «[».
        // Релаксация словных клавиш: буква в чужой раскладке продолжает
        // слово, гипотеза EN с «[» топится скорингом — переключаемся на RU.
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        let ds = type_word(
            &mut p,
            "c[tfv",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert_eq!(
            switch_only(&ds),
            vec![Decision::SwitchAndFix {
                layout: Layout::Ru,
                replacement: "схе".into(),
                typed: 3,
            }]
        );
    }

    #[test]
    fn case_preserved_in_replacement() {
        let mut p = pred(PredictorConfig::default());
        let mut now = t0();
        // «Ghb» с большой буквы -> замена «При».
        let ds = type_word(
            &mut p,
            "Ghb",
            Layout::En,
            &mut now,
            Duration::from_millis(50),
        );
        assert!(matches!(
            &ds[2],
            Decision::SwitchAndFix { replacement, .. } if replacement == "При"
        ));
    }

    /// Сценарий из спецификации на РЕАЛЬНЫХ встроенных словарях
    /// (assets/lang/*.tsv, без сети и macOS): «dftnktq» -> RU.
    #[test]
    fn real_dicts_dftnktq_switches_to_ru() {
        let mut p = Predictor::new(LangPair::load_embedded(), PredictorConfig::default());
        let km = StaticKeymap;
        let mut now = Instant::now();
        let mut decision = Decision::None;
        for c in "dftnktq".chars() {
            let code = keycode_for(Layout::En, c).expect("код клавиши");
            now += Duration::from_millis(50);
            let d = p.feed_key(code, false, Layout::En, false, &km, now);
            if !matches!(d, Decision::None) {
                decision = d;
            }
        }
        assert!(
            matches!(
                decision,
                Decision::SwitchAndFix {
                    layout: Layout::Ru,
                    ..
                }
            ),
            "ожидалось переключение на RU: {decision:?}"
        );
    }
}

#[cfg(test)]
mod calibrate_real {
    use super::*;
    use crate::keymap::{keycode_for, StaticKeymap};

    #[test]
    #[ignore] // запускать явно: cargo test -- --ignored calibrate
    fn print_real_dict_margins() {
        let langs = LangPair::load_embedded();
        let cfg = PredictorConfig::default();
        let mut p = Predictor::new(langs, cfg);
        let km = StaticKeymap;
        let mut now = Instant::now();
        for (word, layout) in [
            ("dftnktq", Layout::En),
            ("ghbdtn", Layout::En),
            ("jdk", Layout::En),
            ("qwe", Layout::En),
            ("abc", Layout::En),
            ("ghbdtn", Layout::Ru),
            ("shspev", Layout::En),
            ("vjybnth", Layout::En),
        ] {
            p.forget_last();
            now += Duration::from_secs(10);
            print!("=== {word} ({layout:?}): ");
            for c in word.chars() {
                let code = keycode_for(Layout::En, c).unwrap();
                now += Duration::from_millis(50);
                let d = p.feed_key(code, false, layout, false, &km, now);
                if !matches!(d, Decision::None) {
                    print!(" -> {d:?}");
                }
            }
            p.forget_last();
            println!();
        }
    }
}
