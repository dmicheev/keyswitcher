//! Конвейер режима печати: event tap, предиктор, ретро-фикс, хоткей
//! и спеллчек-подсказки (попап у каретки).
//!
//! [`TypingPipeline`] ставит на main run loop активный CGEventTap
//! (keyDown + flagsChanged), пропускает события через [`Predictor`] и при
//! уверенном определении языка переключает раскладку с исправлением уже
//! набранного префикса (backspace × k + unicode-событие). Отдельно
//! обрабатывается горячая клавиша конвертации: при активном выделении
//! конвертируется весь выделенный текст (через AX + ввод поверх выделения),
//! иначе — последнее слово. Работает и при выключенном `typing_detect`. По завершении слова
//! разделителем (пробел) неизвестное слово проверяется спеллчеком
//! ([`crate::spell`]) и при наличии вариантов показывается попап
//! [`SuggestPanel`]: цифры 1–9 применяют вариант, Esc закрывает.
//!
//! Свойства:
//! * свои пост-события пропускаются по `kCGEventSourceStateID != HID(1)`;
//! * события с модификаторами (кроме Shift) не анализируются и не меняются;
//! * Secure Input / отключение тапа системой → молчаливая деградация
//!   (ре-включение тапа, ничего не переключаем);
//! * первый запуск без разрешения Accessibility: конвейер живёт без тапа,
//!   `tick` периодически пытается его создать — после выдачи разрешения
//!   в «Универсальном доступе» режим печати включается без перезапуска;
//! * приложения из `exclude_apps` и собственное окно настроек не трогаем.

use std::time::{Duration, Instant};

use crate::config::{Config, Hotkey, HotkeyMods};
use crate::engine::{Layout, System};
use crate::keymap::codes;
use crate::layout_convert::LayoutConverter;
use crate::macos::ffi;
use crate::macos::keymap::RuntimeKeymap;
use crate::macos::pasteboard;
use crate::macos::suggest_panel::SuggestPanel;
use crate::macos::system::{ax_selected_text, MacSystem};
use crate::predictor::{CommittedWord, Decision, Predictor, PredictorConfig};
use crate::spell::{self, SpellConfig, SuggestionKind};

/// Маска событий тапа: keyDown + flagsChanged.
const TAP_MASK: ffi::CGEventMask =
    (1 << ffi::KCG_EVENT_KEY_DOWN as u64) | (1 << ffi::KCG_EVENT_FLAGS_CHANGED as u64);
/// Лимит UTF-16-единиц на одно unicode-событие (поле события ограничено ~20).
const UNICODE_CHUNK: usize = 20;
/// Время жизни попапа подсказок без взаимодействия.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Задержка конвертации выделения после нажатия хоткея: колбэк тапа
/// возвращается сразу, а AX-обмен идёт уже вне тапа (на main run loop).
const CONVERT_DELAY: f64 = 0.05;
/// Верхняя граница размера выделения для конвертации: замена длинного текста
/// посимвольным вводом становится заметно медленной.
const MAX_SELECTION_CHARS: usize = 10_000;
/// Ожидание копирования (Cmd+C → смена буфера): не изменился — выделения нет.
const CLIP_COPY_TIMEOUT: Duration = Duration::from_millis(400);
/// Интервал опроса буфера обмена в фазе копирования/восстановления.
const CLIP_POLL_INTERVAL: f64 = 0.03;
/// Задержка перед восстановлением буфера после Cmd+V (приложение читает
/// буфер асинхронно — ранняя замена вставила бы старый текст).
const CLIP_RESTORE_DELAY: Duration = Duration::from_millis(350);

/// Режим буферного конвейера конвертации выделения.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipPhase {
    /// Скопировали (Cmd+C), ждём изменения буфера.
    Copy,
    /// Записали конверт, отправили Cmd+V, ждём вставки перед восстановлением.
    Paste,
}

/// Состояние асинхронной конвертации выделения через буфер обмена.
struct ClipConvert {
    phase: ClipPhase,
    /// Буфер до копирования: строка — восстановим после вставки,
    /// `None` — не-строковое содержимое (не умеем сохранять).
    old_string: Option<String>,
    /// Счётчик изменений до нашего Cmd+C.
    old_count: isize,
    /// Счётчик после нашей записи конверта (защита восстановления).
    our_count: isize,
    /// Дедлайн текущей фазы (ожидание копирования / восстановление).
    deadline: Instant,
}

/// «Рецепт» применения варианта исправления.
#[derive(Debug, Clone)]
struct SuggestFix {
    /// Сколько backspace нажать (слово + разделитель).
    backspaces: usize,
    /// Что ввести (исправление + разделитель).
    text: String,
    /// Целевая раскладка для варианта-конвертации (`None` — не переключать).
    layout: Option<Layout>,
}

/// Режим печати: тап + предиктор + постинг событий + попап подсказок.
pub struct TypingPipeline {
    predictor: Predictor,
    keymap: RuntimeKeymap,
    /// Конвертер текста EN↔RU (для выделенного текста).
    converter: LayoutConverter,
    system: MacSystem,
    /// Проекция конфигурации, обновляется из tick().
    typing_detect: bool,
    retro_fix: bool,
    hotkey: Hotkey,
    /// Настройки спеллчека (проекция конфигурации).
    spell: SpellConfig,
    /// Попап подсказок (создаётся вместе с конвейером, скрыт).
    suggest_panel: SuggestPanel,
    /// «Рецепты» исправлений для показанного попапа.
    pending_fixes: Vec<SuggestFix>,
    /// Зеркало видимости попапа (тап и AppKit живут на main run loop).
    panel_visible: bool,
    /// Когда показан попап (таймаут).
    panel_shown_at: Option<Instant>,
    /// Приложение, для которого показан попап (скрытие при смене).
    panel_app: Option<(String, String)>,
    /// Кэш активного приложения из tick() — AX-вызовы в колбэке тапа
    /// запрещены (синхронный обмен с таймаутом 0.25 c тормозит печать).
    current_app: Option<(String, String)>,
    /// CGEventSource для наших пост-событий (CombinedSessionState).
    posting_source: ffi::CGEventSourceRef,
    /// Порт тапа (для ре-включения после DisabledByTimeout).
    tap_port: ffi::CFMachPortRef,
    /// Run-loop source тапа (чтобы снять при остановке).
    tap_source: ffi::CFRunLoopSourceRef,
    /// Текущие флаги-модификаторы (из flagsChanged).
    flags: ffi::CGEventFlags,
    /// Активное окно в исключениях или это мы сами → не вмешиваемся.
    excluded: bool,
    /// Последняя раскладка, которую мы видели/применяли (детект ручной смены).
    last_layout: Option<Layout>,
    /// Имя нашего бинарника (детект собственного окна настроек).
    own_exe: Option<String>,
    /// «Взведён» ли тап модификатора (модификаторы нажаты без других клавиш).
    mod_tap_armed: bool,
    /// Таймер конвертации: one-shot (отложенный старт после хоткея) либо
    /// повторяющийся (опрос буфера обмена); `null` — не запланирован.
    convert_timer: ffi::CFRunLoopTimerRef,
    /// Активная конвертация выделения через буфер обмена (`None` — нет).
    clip: Option<ClipConvert>,
}

impl TypingPipeline {
    /// Создаёт конвейер и вешает тап на main run loop. Всегда возвращает
    /// конвейер: если тап создать не удалось (обычно первый запуск без
    /// разрешения Accessibility), он работает в деградировавшем режиме,
    /// а [`Self::tick`] повторяет попытку, пока разрешение не выдадут.
    pub fn install(config: &Config) -> Option<Box<Self>> {
        let posting_source =
            unsafe { ffi::CGEventSourceCreate(ffi::KCG_EVENT_SOURCE_STATE_COMBINED) };
        if posting_source.is_null() {
            log::warn!("Режим печати: не удалось создать CGEventSource");
            return None;
        }

        // Конвейер сначала (колбэку нужен указатель на него как userInfo),
        // поля тапа заполняем по мере создания ресурсов; тап не сработает
        // раньше, чем его source добавлен в run loop. Владение ресурсами —
        // у структуры: в путях ошибки Drop освобождает только созданное
        // (null-поля пропускаются), ручных CFRelease здесь нет.
        let keymap = RuntimeKeymap::build();
        let converter = LayoutConverter::new(&keymap);
        let mut pipeline = Box::new(Self {
            predictor: Predictor::new(
                crate::predictor::LangPair::load_embedded(),
                predictor_cfg(config),
            ),
            keymap,
            converter,
            system: MacSystem::new(),
            typing_detect: config.typing_detect,
            retro_fix: config.retro_fix,
            hotkey: config.convert_hotkey,
            spell: spell_cfg(config),
            suggest_panel: SuggestPanel::new(),
            pending_fixes: Vec::new(),
            panel_visible: false,
            panel_shown_at: None,
            panel_app: None,
            current_app: None,
            posting_source,
            tap_port: core::ptr::null_mut(),
            tap_source: core::ptr::null_mut(),
            flags: 0,
            excluded: false,
            last_layout: None,
            own_exe: std::env::current_exe()
                .ok()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())),
            mod_tap_armed: false,
            convert_timer: core::ptr::null_mut(),
            clip: None,
        });
        // Клик по кнопке попапа должен попасть в конвейер: адрес аллокации
        // Box стабилен, пока конвейер жив.
        let pipe_ptr: *mut TypingPipeline = pipeline.as_mut();
        pipeline.suggest_panel.set_pipe(pipe_ptr);

        // Тап: активный (может менять/поглощать события). При первом
        // запуске без разрешения Accessibility создание не удастся —
        // тогда конвейер остаётся без тапа, а tick() будет повторять
        // попытку, пока разрешение не выдадут (без перезапуска).
        if !pipeline.try_install_tap() {
            log::warn!("Режим печати: тап не создан — жду разрешение Accessibility");
        }
        Some(pipeline)
    }

    /// Пытается создать event tap и добавить его source в main run loop.
    /// Вызывается из [`Self::install`] и повторно из [`Self::tick`], пока
    /// тап не появится: `CGEventTapCreate` возвращает NULL без разрешения
    /// Accessibility, а после его выдачи в System Settings тап начинает
    /// работать у уже запущенного процесса — достаточно создать его заново.
    /// `false` — тапа по-прежнему нет (весь режим печати, включая хоткей,
    /// остаётся выключенным до следующей попытки).
    fn try_install_tap(&mut self) -> bool {
        if !self.tap_port.is_null() {
            return true;
        }
        let user_info: *mut TypingPipeline = self;

        // Активный тап (может менять/поглощать события — запас на будущее;
        // сейчас события мы не модифицируем). При отказе — слушающий.
        let mut listen_only = false;
        let mut tap_port = unsafe {
            ffi::CGEventTapCreate(
                ffi::KCG_SESSION_EVENT_TAP,
                ffi::KCG_HEAD_INSERT_EVENT_TAP,
                ffi::KCG_EVENT_TAP_OPTION_DEFAULT,
                TAP_MASK,
                tap_callback,
                user_info.cast(),
            )
        };
        if tap_port.is_null() {
            listen_only = true;
            tap_port = unsafe {
                ffi::CGEventTapCreate(
                    ffi::KCG_SESSION_EVENT_TAP,
                    ffi::KCG_HEAD_INSERT_EVENT_TAP,
                    ffi::KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    TAP_MASK,
                    tap_callback,
                    user_info.cast(),
                )
            };
        }
        if tap_port.is_null() {
            return false;
        }
        self.tap_port = tap_port;
        let tap_source =
            unsafe { ffi::CFMachPortCreateRunLoopSource(core::ptr::null_mut(), tap_port, 0) };
        if tap_source.is_null() {
            log::warn!("Режим печати: run-loop source не создан");
            return false;
        }
        self.tap_source = tap_source;
        unsafe {
            ffi::CFRunLoopAddSource(
                ffi::CFRunLoopGetMain(),
                tap_source,
                ffi::kCFRunLoopCommonModes,
            );
        }
        log::info!(
            "Режим печати: тап установлен ({})",
            if listen_only { "listen-only" } else { "active" }
        );
        true
    }

    /// Периодический тик из поллинга (UI NSTimer / headless CFRunLoopTimer):
    /// обновляет конфиг, исключения, дотягивает тап после выдачи разрешения
    /// Accessibility и следит за попапом подсказок (смена приложения, таймаут).
    pub fn tick(&mut self, system: &mut MacSystem, config: &Config) {
        // Тап мог не создаться на старте (первый запуск без разрешения
        // Accessibility) — пробуем, пока пользователь его не выдаст.
        if self.tap_port.is_null() {
            self.try_install_tap();
        }
        self.typing_detect = config.typing_detect;
        self.retro_fix = config.retro_fix;
        self.hotkey = config.convert_hotkey;
        self.spell = spell_cfg(config);
        // Хоткей работает независимо от typing_detect, поэтому слова
        // отслеживаются всегда; автопереключение (включая карту коротких
        // слов) выключено флагом enabled.
        self.predictor.update_config(predictor_cfg(config));

        // Исключения: приложение из exclude_apps или мы сами (окно настроек).
        let mut active: Option<(String, String)> = None;
        self.excluded = false;
        if let Some(win) = system.active_window() {
            self.excluded = is_own_app(&win.app_id, &win.app_name, self.own_exe.as_deref())
                || config.is_excluded(&win.app_id)
                || config.is_excluded(&win.app_name);
            active = Some((win.app_id, win.app_name));
        }
        self.current_app = active.clone();

        // Попап подсказок: закрываем при исключении, таймауте и смене
        // приложения (исправление «в чужое окно» бессмысленно).
        if self.panel_visible {
            let app_changed = match (&self.panel_app, &active) {
                (Some((id, name)), Some((cur_id, cur_name))) => cur_id != id && cur_name != name,
                (None, Some(_)) => false, // приложение ещё не зафиксировано
                _ => true,
            };
            let timed_out = self
                .panel_shown_at
                .is_some_and(|t| t.elapsed() > SUGGEST_TIMEOUT);
            if self.excluded || timed_out || app_changed {
                self.hide_suggest_panel();
            }
        }

        // Watchdog: тап мог быть выключен системой (Secure Input, таймаут).
        // Восстановление делаем здесь, а не только в колбэке.
        if !self.tap_port.is_null() {
            unsafe {
                ffi::CGEventTapEnable(self.tap_port, true);
            }
        }
    }

    /// Обработка keyDown. Возвращает событие (не меняем никогда).
    fn on_key_down(&mut self, event: ffi::CGEventRef) -> ffi::CGEventRef {
        // 1. Только «аппаратные» события: наши посты имеют другой sourceStateID.
        let state_id =
            unsafe { ffi::CGEventGetIntegerValueField(event, ffi::KCG_EVENT_SOURCE_STATE_ID) };
        if state_id != i64::from(ffi::KCG_EVENT_SOURCE_STATE_HID) {
            return event;
        }
        // 2. Автоповторы не анализируем.
        if unsafe { ffi::CGEventGetIntegerValueField(event, ffi::KCG_KEYBOARD_EVENT_AUTOREPEAT) }
            == 1
        {
            return event;
        }

        self.flags = unsafe { ffi::CGEventGetFlags(event) };
        let keycode =
            unsafe { ffi::CGEventGetIntegerValueField(event, ffi::KCG_KEYBOARD_EVENT_KEYCODE) };
        let Ok(keycode) = u8::try_from(keycode) else {
            return event;
        };

        // 3. Открытый попап подсказок: цифра 1–9 применяет вариант, Esc
        // закрывает; обе поглощаются. Любая другая клавиша закрывает
        // попап и пропускается дальше (печать не прерывается).
        if self.panel_visible {
            if keycode == codes::ESCAPE {
                self.hide_suggest_panel();
                return core::ptr::null_mut();
            }
            let disallowed = ffi::KCG_FLAG_CONTROL
                | ffi::KCG_FLAG_OPTION
                | ffi::KCG_FLAG_COMMAND
                | ffi::KCG_FLAG_NUMERIC_PAD
                | ffi::KCG_FLAG_SHIFT;
            if self.flags & disallowed == 0 {
                if let Some(index) = digit_index(keycode).filter(|&i| i < self.pending_fixes.len())
                {
                    self.apply_suggest_fix(index);
                    return core::ptr::null_mut();
                }
            }
            self.hide_suggest_panel();
            // Событие не поглощаем — обычная печать продолжается.
        }

        // 4. Хоткей конвертации работает всегда (модификаторы сверяем точно).
        if self.hotkey_matches(keycode) {
            log::debug!("хоткей: нажатие распознано (keycode={keycode:#x})");
            self.request_convert();
            return event;
        }

        // Любая аппаратная клавиша «погашает» взведённый тап модификатора.
        if self.hotkey.is_mod_only() {
            self.mod_tap_armed = false;
        }

        // 5. В исключениях не вмешиваемся — и слова не запоминаем
        // (иначе хоткей мог бы конвертировать слово, набранное в
        // исключённом приложении, уже из чужого окна).
        // При typing_detect=false слова всё же трекаются — для хоткея;
        // сами решения подавлены enabled=false (см. tick).
        if self.excluded {
            self.predictor.forget_last();
            self.hide_suggest_panel();
            return event;
        }

        // 6. Модификаторы кроме Shift (и Caps) — не печатаем: сброс слова.
        let disallowed = ffi::KCG_FLAG_CONTROL
            | ffi::KCG_FLAG_OPTION
            | ffi::KCG_FLAG_COMMAND
            | ffi::KCG_FLAG_NUMERIC_PAD;
        if self.flags & disallowed != 0 {
            self.predictor.reset_word();
            return event;
        }

        // 7. Определяем текущую раскладку и ручную смену.
        let current_layout = self.current_layout();
        let manual_change = match (self.last_layout, current_layout) {
            (Some(prev), cur) => prev != cur,
            (None, _) => false,
        };
        self.last_layout = Some(current_layout);
        if current_layout == Layout::Other {
            self.predictor.reset_word();
            return event;
        }

        // 8. Печатаем в предиктор (Shift XOR Caps — фактический регистр).
        let shift = self.flags & ffi::KCG_FLAG_SHIFT != 0;
        let caps = self.flags & ffi::KCG_FLAG_ALPHA_SHIFT != 0;
        let decision = self.predictor.feed_key(
            keycode,
            shift ^ caps,
            current_layout,
            manual_change,
            &self.keymap,
            Instant::now(),
        );

        // 9. Решение: переключение + ретро-фикс; иначе — спеллчек
        // зафиксированного слова (попап у каретки).
        let committed = self.predictor.take_committed();
        if let Decision::SwitchAndFix {
            layout,
            replacement,
            typed,
        } = decision
        {
            self.apply_switch(current_layout, layout, &replacement, typed);
        } else if let Some(word) = committed {
            self.maybe_suggest(&word, current_layout);
        }
        event
    }

    /// Спеллчек зафиксированного слова и показ попапа при наличии вариантов.
    fn maybe_suggest(&mut self, word: &CommittedWord, current: Layout) {
        self.pending_fixes.clear();
        if !self.spell.enabled || word.sep != Some(' ') || word.auto_switched {
            return;
        }
        let (typed_view, other_view) = match current {
            Layout::En => (word.view_en.as_deref(), word.view_ru.as_deref()),
            Layout::Ru => (word.view_ru.as_deref(), word.view_en.as_deref()),
            Layout::Other => return,
        };
        let Some(typed_view) = typed_view else {
            return;
        };
        let suggestions =
            spell::suggestions(self.predictor.langs(), typed_view, other_view, &self.spell);
        if suggestions.is_empty() {
            return;
        }

        // «Рецепты»: стереть слово+пробел, ввести вариант+пробел.
        // Конвертации раскладки — с переключением (как хоткей конвертации).
        let sep = word.sep.unwrap_or(' ');
        self.pending_fixes = suggestions
            .iter()
            .map(|s| SuggestFix {
                backspaces: word.typed + 1,
                text: format!("{}{}", s.text, sep),
                layout: (s.kind == SuggestionKind::Layout).then_some(match current {
                    Layout::En => Layout::Ru,
                    _ => Layout::En,
                }),
            })
            .collect();
        let labels: Vec<String> = suggestions
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}  {}", i + 1, s.text))
            .collect();

        self.suggest_panel.show(&labels);
        self.panel_visible = true;
        self.panel_shown_at = Some(Instant::now());
        self.panel_app = self.current_app.clone();
        log::info!(
            "Режим печати: попап подсказок (вариантов: {})",
            labels.len()
        );
    }

    /// Прячет попап подсказок и забывает «рецепты».
    fn hide_suggest_panel(&mut self) {
        if self.panel_visible {
            self.suggest_panel.hide();
            self.panel_visible = false;
        }
        self.pending_fixes.clear();
        self.panel_shown_at = None;
        self.panel_app = None;
    }

    /// Применяет вариант исправления из попапа (клик мышью или цифра).
    pub(crate) fn apply_suggest_fix(&mut self, index: usize) {
        let Some(fix) = self.pending_fixes.get(index).cloned() else {
            return;
        };
        self.hide_suggest_panel();
        if let Some(target) = fix.layout {
            if !self.system.set_layout(target) {
                log::warn!("подсказка: не удалось переключить раскладку на {target:?}");
            }
            // Наше переключение не должно выглядеть ручной сменой.
            self.last_layout = Some(target);
        }
        unsafe {
            self.post_backspaces(fix.backspaces);
            self.post_unicode(&fix.text);
        }
        self.predictor.forget_last();
        log::info!("Режим печати: подсказка применена");
    }

    /// Хоткей конвертации: при активном выделении — конвертация всего
    /// выделенного текста (Punto-стиль), иначе — последнего слова.
    /// AX-обмен в колбэке тапа недопустим (синхронный обмен тормозит печать),
    /// поэтому работа планируется one-shot таймером main run loop.
    fn request_convert(&mut self) {
        if !self.convert_timer.is_null() || self.clip.is_some() {
            return; // конвертация уже идёт
        }
        if self.arm_convert_timer(CONVERT_DELAY, 0.0) {
            return;
        }
        self.convert_selection_or_word();
    }

    /// Создаёт таймер конвертации (`interval == 0` — one-shot) и добавляет
    /// в main run loop. `false` — создать не удалось (конвертация выполняется
    /// синхронно вызывающим).
    fn arm_convert_timer(&mut self, fire_delay: f64, interval: f64) -> bool {
        if !self.convert_timer.is_null() {
            return true;
        }
        let info = self as *const TypingPipeline as *mut core::ffi::c_void;
        let context = ffi::CFRunLoopTimerContext {
            version: 0,
            info,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let timer = unsafe {
            ffi::CFRunLoopTimerCreate(
                core::ptr::null_mut(),
                ffi::CFAbsoluteTimeGetCurrent() + fire_delay,
                interval,
                0,
                0,
                convert_timer_callback,
                &context,
            )
        };
        if timer.is_null() {
            return false;
        }
        unsafe {
            ffi::CFRunLoopAddTimer(ffi::CFRunLoopGetMain(), timer, ffi::kCFRunLoopCommonModes);
        }
        self.convert_timer = timer;
        true
    }

    /// Останавливает и освобождает таймер конвертации.
    fn stop_convert_timer(&mut self) {
        if self.convert_timer.is_null() {
            return;
        }
        unsafe {
            ffi::CFRunLoopTimerInvalidate(self.convert_timer);
            ffi::CFRelease(self.convert_timer as ffi::CFTypeRef);
        }
        self.convert_timer = core::ptr::null_mut();
    }

    /// Срабатывание таймера конвертации (main run loop, вне колбэка тапа).
    fn on_convert_timer(&mut self) {
        if self.clip.is_some() {
            self.on_clip_poll();
        } else {
            // Первый (отложенный) выстрел: пробуем выделение.
            self.convert_selection_or_word();
        }
    }

    /// Конвертация выделения; при отсутствии выделения — последнего слова.
    fn convert_selection_or_word(&mut self) {
        if self.convert_selection() {
            return;
        }
        if self.start_clip_convert() {
            return;
        }
        self.convert_last_word();
    }

    /// Конвертация выделенного текста в другую раскладку: читаем выделение
    /// через AX, конвертируем посимвольно и вводим поверх (набор заменяет
    /// выделение — без буфера обмена). `true` — выделение сконвертировано.
    fn convert_selection(&mut self) -> bool {
        if self.excluded {
            return false;
        }
        let Some(pid) = self.system.frontmost_pid() else {
            return false;
        };
        let Some(selected) = ax_selected_text(pid) else {
            log::debug!("выделение: недоступно через AX — пробую буфер обмена");
            return false;
        };
        let chars = selected.chars().count();
        if chars > MAX_SELECTION_CHARS {
            log::debug!("выделение: слишком длинное ({chars} символов), пропускаю");
            return false;
        }
        self.apply_converted(
            &selected,
            &format!("хоткей: конвертация выделения ({chars} симв.)"),
        )
    }

    /// Запускает конвертацию выделения через буфер обмена (для приложений,
    /// где AX выделение не отдаёт: Terminal, Electron и т.п.): Cmd+C → ждём
    /// смену буфера → конвертация → Cmd+V → восстановление буфера.
    /// `true` — конвейер запущен (завершится асинхронно).
    fn start_clip_convert(&mut self) -> bool {
        if self.clip.is_some() || self.excluded {
            return false;
        }
        let Some(count) = pasteboard::change_count() else {
            log::debug!("выделение: буфер обмена недоступен");
            return false;
        };
        self.clip = Some(ClipConvert {
            phase: ClipPhase::Copy,
            old_string: pasteboard::string(),
            old_count: count,
            our_count: count,
            deadline: Instant::now() + CLIP_COPY_TIMEOUT,
        });
        unsafe { self.post_cmd_key(codes::KEY_C) };
        if !self.arm_convert_timer(CLIP_POLL_INTERVAL, CLIP_POLL_INTERVAL) {
            self.clip = None;
            return false;
        }
        log::debug!("выделение: копирую через буфер обмена (Cmd+C)");
        true
    }

    /// Очередной опрос буферного конвейера: фаза Copy — ждём содержимое
    /// выделения; фаза Paste — ждём вставки и восстанавливаем буфер.
    fn on_clip_poll(&mut self) {
        let Some(mut clip) = self.clip.take() else {
            self.stop_convert_timer();
            return;
        };
        match clip.phase {
            ClipPhase::Copy => {
                let count = pasteboard::change_count().unwrap_or(clip.old_count);
                if count != clip.old_count {
                    let copied = pasteboard::string();
                    match copied.as_deref().and_then(|s| self.converter.convert(s)) {
                        Some((converted, target)) => {
                            if !pasteboard::set_string(&converted) {
                                log::warn!("выделение: не удалось записать буфер обмена");
                                self.stop_convert_timer();
                                return;
                            }
                            clip.our_count = pasteboard::change_count().unwrap_or(count);
                            clip.phase = ClipPhase::Paste;
                            clip.deadline = Instant::now() + CLIP_RESTORE_DELAY;
                            // В терминалах Cmd+V вставляет в позицию каретки,
                            // игнорируя выделение (в отличие от текстовых
                            // редакторов), — сначала удаляем выделение Backspace.
                            // Если приложение сняло выделение при копировании
                            // (редкая экзотика), пострадает один символ перед
                            // кареткой — поэтому список терминалов, а не все
                            // приложения буферного пути.
                            let is_terminal = self
                                .current_app
                                .as_ref()
                                .is_some_and(|(id, name)| is_terminal_like(id, name));
                            if is_terminal {
                                unsafe { self.post_backspaces(1) };
                            }
                            unsafe { self.post_cmd_key(codes::KEY_V) };
                            if !self.system.set_layout(target) {
                                log::warn!(
                                    "выделение: не удалось переключить раскладку на {target:?}"
                                );
                            }
                            // Наше переключение не должно выглядеть ручной сменой.
                            self.last_layout = Some(target);
                            self.predictor.forget_last();
                            log::info!(
                                "хоткей: конвертация выделения через буфер ({}) -> {target:?}",
                                converted.chars().count()
                            );
                            self.clip = Some(clip);
                        }
                        None => {
                            // Выделение есть, но конвертировать нечего; backspace
                            // последнего слова разрушил бы выделение — выходим.
                            log::debug!("выделение: конвертировать нечего");
                            self.stop_convert_timer();
                        }
                    }
                } else if Instant::now() >= clip.deadline {
                    // Буфер не изменился — выделения нет: обычная конвертация слова.
                    log::debug!("выделение: буфер не изменился — нет выделения");
                    self.stop_convert_timer();
                    self.convert_last_word();
                } else {
                    self.clip = Some(clip);
                }
            }
            ClipPhase::Paste => {
                if Instant::now() >= clip.deadline {
                    // Восстанавливаем прежний буфер, если после нашей записи
                    // туда больше ничего не писали.
                    if let Some(old) = clip.old_string {
                        if pasteboard::change_count() == Some(clip.our_count) {
                            pasteboard::set_string(&old);
                            log::debug!("выделение: буфер обмена восстановлен");
                        }
                    }
                    self.stop_convert_timer();
                } else {
                    self.clip = Some(clip);
                }
            }
        }
    }

    /// Конвертирует `selected` и вводит поверх выделения; общая часть
    /// AX-пути. `label` — для журнала. `true` — сконвертировано.
    fn apply_converted(&mut self, selected: &str, label: &str) -> bool {
        let Some((converted, target)) = self.converter.convert(selected) else {
            log::debug!("выделение: конвертировать нечего");
            return false;
        };
        unsafe { self.post_unicode(&converted) };
        if !self.system.set_layout(target) {
            log::warn!("выделение: не удалось переключить раскладку на {target:?}");
        }
        // Наше переключение не должно выглядеть ручной сменой.
        self.last_layout = Some(target);
        self.predictor.forget_last();
        log::info!("{label} -> {target:?}");
        true
    }

    /// Хоткей-конвертация последнего слова в другую раскладку
    /// (с переключением раскладки — как Punto).
    fn convert_last_word(&mut self) {
        let current = self.current_layout();
        let target = match current {
            Layout::En => Layout::Ru,
            _ => Layout::En,
        };
        let Some(conv) = self.predictor.convert_last_word(current) else {
            log::debug!("хоткей: конвертировать нечего");
            return;
        };
        unsafe {
            self.post_backspaces(conv.backspaces);
            self.post_unicode(&conv.text);
        }
        if !self.system.set_layout(target) {
            log::warn!("хоткей: не удалось переключить раскладку на {target:?}");
        }
        // Наше переключение не должно выглядеть ручной сменой.
        self.last_layout = Some(target);
        self.predictor.forget_last();
        log::info!("хоткей: конвертация {current:?} -> {target:?}");
    }

    /// Переключение раскладки и ретро-фикс префикса.
    fn apply_switch(&mut self, from: Layout, to: Layout, replacement: &str, typed: usize) {
        if !self.system.set_layout(to) {
            log::warn!("Режим печати: не удалось переключить раскладку на {to:?}");
            return;
        }
        // Наше переключение не должно выглядеть ручной сменой.
        self.last_layout = Some(to);
        if self.retro_fix {
            unsafe {
                self.post_backspaces(typed);
                self.post_unicode(replacement);
            }
            log::info!("Режим печати: {from:?} -> {to:?} (+ретро-фикс)");
        } else {
            log::info!("Режим печати: {from:?} -> {to:?}");
        }
    }

    /// Текущая раскладка через TIS.
    fn current_layout(&mut self) -> Layout {
        self.system.current_layout()
    }

    fn hotkey_matches(&self, keycode: u8) -> bool {
        if self.hotkey.key != Some(keycode) {
            return false;
        }
        mods_match(self.hotkey.mods, self.flags)
    }

    /// Обработка flagsChanged: трекинг модификаторов и хоткей «тап
    /// модификатора» — срабатывает на чистом отпускании (без других клавиш).
    fn on_flags_changed(&mut self, event: ffi::CGEventRef) -> ffi::CGEventRef {
        let prev = self.flags;
        let cur = unsafe { ffi::CGEventGetFlags(event) };
        self.flags = cur;
        if self.hotkey.is_mod_only() {
            let was_exact = mods_match(self.hotkey.mods, prev);
            let now_exact = mods_match(self.hotkey.mods, cur);
            if now_exact && !was_exact {
                // Модификаторы нажаты ровно в целевой комбинации.
                self.mod_tap_armed = true;
            } else if was_exact && !now_exact {
                // Ушли из целевой комбинации: чистое отпускание -> триггер;
                // добавление лишнего модификатора — просто гасим.
                let any_hotkey_mod = cur & hotkey_mod_mask(self.hotkey.mods) != 0;
                if self.mod_tap_armed && !any_hotkey_mod {
                    log::debug!("хоткей: тап модификатора");
                    self.request_convert();
                }
                self.mod_tap_armed = false;
            }
        }
        event
    }

    /// Backspace × n (keyDown/keyUp пары) через tagged source.
    ///
    /// # Safety
    /// Постинг событий; source создан в `install`.
    unsafe fn post_backspaces(&self, n: usize) {
        for _ in 0..n {
            for &down in &[true, false] {
                let ev = ffi::CGEventCreateKeyboardEvent(
                    self.posting_source,
                    u16::from(codes::BACKSPACE),
                    down,
                );
                if !ev.is_null() {
                    ffi::CGEventPost(ffi::KCG_SESSION_EVENT_TAP, ev);
                    ffi::CFRelease(ev as ffi::CFTypeRef);
                }
            }
        }
    }

    /// Ввод строки одним unicode-событием (ключевая пара keyDown/keyUp).
    /// Длинные строки режутся на куски по ~20 UTF-16 единиц.
    ///
    /// # Safety
    /// Постинг событий; source создан в `install`.
    unsafe fn post_unicode(&self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        for chunk in units.chunks(UNICODE_CHUNK) {
            for &down in &[true, false] {
                let ev = ffi::CGEventCreateKeyboardEvent(self.posting_source, 0, down);
                if ev.is_null() {
                    continue;
                }
                ffi::CGEventKeyboardSetUnicodeString(ev, chunk.len(), chunk.as_ptr());
                ffi::CGEventPost(ffi::KCG_SESSION_EVENT_TAP, ev);
                ffi::CFRelease(ev as ffi::CFTypeRef);
            }
        }
    }

    /// Нажатие клавиши с Cmd (Cmd+C / Cmd+V) — пара keyDown/keyUp.
    ///
    /// # Safety
    /// Постинг событий; source создан в `install`.
    unsafe fn post_cmd_key(&self, keycode: u8) {
        for &down in &[true, false] {
            let ev = ffi::CGEventCreateKeyboardEvent(self.posting_source, u16::from(keycode), down);
            if ev.is_null() {
                continue;
            }
            ffi::CGEventSetFlags(ev, ffi::KCG_FLAG_COMMAND);
            ffi::CGEventPost(ffi::KCG_SESSION_EVENT_TAP, ev);
            ffi::CFRelease(ev as ffi::CFTypeRef);
        }
    }
}

impl Drop for TypingPipeline {
    fn drop(&mut self) {
        // Null-гарды: в путях ошибки install() часть ресурсов могла быть
        // не создана — освобождаем только существующие (CFRelease(NULL)
        // падает, а CFRunLoopRemoveSource с NULL просто недопустима).
        unsafe {
            if !self.convert_timer.is_null() {
                ffi::CFRunLoopTimerInvalidate(self.convert_timer);
                ffi::CFRelease(self.convert_timer as ffi::CFTypeRef);
            }
            if !self.tap_source.is_null() {
                ffi::CFRunLoopRemoveSource(
                    ffi::CFRunLoopGetMain(),
                    self.tap_source,
                    ffi::kCFRunLoopCommonModes,
                );
                ffi::CFRelease(self.tap_source as ffi::CFTypeRef);
            }
            if !self.tap_port.is_null() {
                ffi::CGEventTapEnable(self.tap_port, false);
                ffi::CFRelease(self.tap_port as ffi::CFTypeRef);
            }
            if !self.posting_source.is_null() {
                ffi::CFRelease(self.posting_source as ffi::CFTypeRef);
            }
        }
    }
}

/// Колбэк таймера конвертации (C ABI). One-shot (interval == 0) — отложенный
/// старт: снимаем таймер, возвращаем владение и конвертируем. Повторяющийся —
/// опрос буферного конвейера: таймер продолжает тикать, остановит его сам
/// `on_clip_poll` через поле `convert_timer`.
extern "C" fn convert_timer_callback(timer: ffi::CFRunLoopTimerRef, info: *mut core::ffi::c_void) {
    if info.is_null() {
        return;
    }
    // Safety: указатель валиден, пока жив TypingPipeline (main-поток).
    let pipeline: &mut TypingPipeline = unsafe { &mut *(info as *mut TypingPipeline) };
    let one_shot = unsafe { ffi::CFRunLoopTimerGetInterval(timer) } == 0.0;
    if one_shot {
        pipeline.convert_timer = core::ptr::null_mut();
        unsafe {
            ffi::CFRunLoopTimerInvalidate(timer);
            ffi::CFRelease(timer as ffi::CFTypeRef);
        }
        pipeline.convert_selection_or_word();
    } else {
        pipeline.on_convert_timer();
    }
}

/// Колбэк тапа (C ABI): быстрый, без блокировок; событие не меняем.
extern "C" fn tap_callback(
    _proxy: ffi::CGEventTapProxy,
    event_type: u32,
    event: ffi::CGEventRef,
    user_info: *mut core::ffi::c_void,
) -> ffi::CGEventRef {
    if user_info.is_null() || event.is_null() {
        return event;
    }
    // Safety: указатель валиден, пока жив TypingPipeline (main-поток).
    let pipeline: &mut TypingPipeline = unsafe { &mut *(user_info as *mut TypingPipeline) };
    match event_type {
        ffi::KCG_EVENT_TAP_DISABLED_BY_TIMEOUT | ffi::KCG_EVENT_TAP_DISABLED_BY_USER_INPUT => {
            // Secure Input/таймаут: молчаливо поднимаем тап обратно.
            unsafe { ffi::CGEventTapEnable(pipeline.tap_port, true) };
            core::ptr::null_mut()
        }
        ffi::KCG_EVENT_KEY_DOWN => pipeline.on_key_down(event),
        ffi::KCG_EVENT_FLAGS_CHANGED => pipeline.on_flags_changed(event),
        _ => event,
    }
}

/// Проекция Config → настройки предиктора.
fn predictor_cfg(config: &Config) -> PredictorConfig {
    PredictorConfig {
        min_prefix: config.min_prefix_chars.max(2),
        switch_threshold: config.switch_threshold.max(1.0),
        ngram_weight: crate::predictor::PredictorConfig::default().ngram_weight,
        enabled: config.typing_detect,
    }
}

/// Проекция Config → настройки спеллчека.
fn spell_cfg(config: &Config) -> SpellConfig {
    SpellConfig {
        enabled: config.spell_suggest,
        min_chars: config.spell_min_chars.max(1),
        // Выбор в попапе — цифры 1–9, больше и не показать.
        max_suggestions: config.spell_max_suggestions.clamp(1, 9),
    }
}

/// Индекс варианта по коду цифровой клавиши верхнего ряда (1..=9 → 0..=8).
fn digit_index(keycode: u8) -> Option<usize> {
    match keycode {
        0x12 => Some(0), // 1
        0x13 => Some(1), // 2
        0x14 => Some(2), // 3
        0x15 => Some(3), // 4
        0x17 => Some(4), // 5
        0x16 => Some(5), // 6
        0x1A => Some(6), // 7
        0x1C => Some(7), // 8
        0x19 => Some(8), // 9
        _ => None,
    }
}

/// Точное совпадение набора модификаторов с флагами события.
fn mods_match(m: HotkeyMods, f: ffi::CGEventFlags) -> bool {
    (f & ffi::KCG_FLAG_CONTROL != 0) == m.ctrl
        && (f & ffi::KCG_FLAG_SHIFT != 0) == m.shift
        && (f & ffi::KCG_FLAG_OPTION != 0) == m.option
        && (f & ffi::KCG_FLAG_COMMAND != 0) == m.cmd
}

/// Маска модификаторов хоткея (для проверки «хоть один ещё нажат»).
fn hotkey_mod_mask(m: HotkeyMods) -> ffi::CGEventFlags {
    let mut mask = 0;
    if m.ctrl {
        mask |= ffi::KCG_FLAG_CONTROL;
    }
    if m.shift {
        mask |= ffi::KCG_FLAG_SHIFT;
    }
    if m.option {
        mask |= ffi::KCG_FLAG_OPTION;
    }
    if m.cmd {
        mask |= ffi::KCG_FLAG_COMMAND;
    }
    mask
}

/// Является ли активное приложение нами самими (диалог настроек):
/// по bundle id, отображаемому имени или имени нашего бинарника.
fn is_own_app(app_id: &str, app_name: &str, own_exe: Option<&str>) -> bool {
    let own_ids = ["smartswitcher", "ru.dimka.smartswitcher"];
    if own_ids.iter().any(|n| app_id.eq_ignore_ascii_case(n)) {
        return true;
    }
    if app_name.eq_ignore_ascii_case("Smartswitcher") {
        return true;
    }
    own_exe.is_some_and(|exe| app_name.eq_ignore_ascii_case(exe))
}

/// Терминалы: Cmd+V вставляет в позицию каретки, не заменяя выделение,
/// поэтому буферному пути нужен Backspace перед вставкой. Определяем по
/// характерным частям bundle id / имени (регистр не важен).
fn is_terminal_like(app_id: &str, app_name: &str) -> bool {
    const MARKS: [&str; 8] = [
        "terminal",
        "iterm",
        "kitty",
        "alacritty",
        "wezterm",
        "warp",
        "hyper",
        "ghostty",
    ];
    let id = app_id.to_ascii_lowercase();
    let name = app_name.to_lowercase();
    MARKS.iter().any(|m| id.contains(m) || name.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_app_detection() {
        assert!(is_own_app("ru.dimka.smartswitcher", "Smartswitcher", None));
        assert!(is_own_app("smartswitcher", "whatever", None));
        assert!(is_own_app(
            "com.apple.Terminal",
            "smartswitcher",
            Some("smartswitcher")
        ));
        assert!(!is_own_app(
            "com.apple.Terminal",
            "Terminal",
            Some("smartswitcher")
        ));
        assert!(!is_own_app(
            "com.apple.Safari",
            "Safari",
            Some("smartswitcher")
        ));
    }

    #[test]
    fn terminal_like_detection() {
        assert!(is_terminal_like("com.apple.Terminal", "Terminal"));
        assert!(is_terminal_like("com.googlecode.iterm2", "iTerm2"));
        assert!(is_terminal_like("net.kovidgoyal.kitty", "kitty"));
        assert!(is_terminal_like("io.alacritty", "Alacritty"));
        assert!(is_terminal_like("dev.warp.Warp-Stable", "Warp"));
        assert!(is_terminal_like("com.mitchellh.ghostty", "Ghostty"));
        // Редакторы и браузеры — не терминалы.
        assert!(!is_terminal_like("com.apple.TextEdit", "TextEdit"));
        assert!(!is_terminal_like("com.google.Chrome", "Chrome"));
        assert!(!is_terminal_like(
            "com.microsoft.VSCode",
            "Visual Studio Code"
        ));
        // «Terminal» внутри чужого имени не должен проходить по пустой строке.
        assert!(!is_terminal_like("", ""));
    }

    #[test]
    fn hotkey_mod_matching() {
        // Проверка сравнения модификаторов через flags.
        let hk = Hotkey {
            mods: HotkeyMods {
                ctrl: true,
                shift: false,
                option: false,
                cmd: false,
            },
            key: Some(codes::F13),
        };
        let f = ffi::KCG_FLAG_CONTROL;
        assert!(mods_match(hk.mods, f));
        assert!(!mods_match(hk.mods, f | ffi::KCG_FLAG_SHIFT));
        assert!(!mods_match(hk.mods, 0));
    }

    /// Тап модификатора: семантика переходов флагов.
    #[test]
    fn mod_tap_transitions() {
        let hk = Hotkey::default(); // option
        assert!(hk.is_mod_only());
        let opt = ffi::KCG_FLAG_OPTION;
        assert_eq!(hotkey_mod_mask(hk.mods), opt);

        // нажали option: не точное -> точное => взводится
        assert!(!mods_match(hk.mods, 0));
        assert!(mods_match(hk.mods, opt));
        // добавили cmd: точное -> не точное, option ещё нажат => без триггера
        let extra = opt | ffi::KCG_FLAG_COMMAND;
        assert!(!mods_match(hk.mods, extra));
        assert!(extra & hotkey_mod_mask(hk.mods) != 0);
        // отпустили всё (flags = 0): mods_match уже false, триггер допустим
    }

    #[test]
    fn top_row_digit_keycodes() {
        for (code, idx) in [
            (0x12, 0), // 1
            (0x13, 1), // 2
            (0x14, 2), // 3
            (0x15, 3), // 4
            (0x17, 4), // 5
            (0x16, 5), // 6
            (0x1A, 6), // 7
            (0x1C, 7), // 8
            (0x19, 8), // 9
        ] {
            assert_eq!(digit_index(code), Some(idx));
        }
        // Ноль и не-цифры варианты не выбирают.
        assert_eq!(digit_index(0x1D), None); // 0
        assert_eq!(digit_index(codes::SPACE), None);
        assert_eq!(digit_index(0x53), None); // numpad 1
    }

    #[test]
    fn spell_cfg_clamps() {
        let cfg = Config {
            spell_suggest: false,
            spell_min_chars: 0,
            spell_max_suggestions: 42,
            ..Config::default()
        };
        let s = spell_cfg(&cfg);
        assert!(!s.enabled);
        assert_eq!(s.min_chars, 1);
        assert_eq!(s.max_suggestions, 9);
    }
}
