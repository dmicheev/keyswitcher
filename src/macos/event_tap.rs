//! Конвейер режима печати: event tap, предиктор, ретро-фикс, хоткей
//! и спеллчек-подсказки (попап у каретки).
//!
//! [`TypingPipeline`] ставит на main run loop активный CGEventTap
//! (keyDown + flagsChanged), пропускает события через [`Predictor`] и при
//! уверенном определении языка переключает раскладку с исправлением уже
//! набранного префикса (backspace × k + unicode-событие). Отдельно
//! обрабатывается горячая клавиша конвертации последнего слова — она
//! работает и при выключенном `typing_detect`. По завершении слова
//! разделителем (пробел) неизвестное слово проверяется спеллчеком
//! ([`crate::spell`]) и при наличии вариантов показывается попап
//! [`SuggestPanel`]: цифры 1–9 применяют вариант, Esc закрывает.
//!
//! Свойства:
//! * свои пост-события пропускаются по `kCGEventSourceStateID != HID(1)`;
//! * события с модификаторами (кроме Shift) не анализируются и не меняются;
//! * Secure Input / отключение тапа системой → молчаливая деградация
//!   (ре-включение тапа, ничего не переключаем);
//! * приложения из `exclude_apps` и собственное окно настроек не трогаем.

use std::time::{Duration, Instant};

use crate::config::{Config, Hotkey, HotkeyMods};
use crate::engine::{Layout, System};
use crate::keymap::codes;
use crate::macos::ffi;
use crate::macos::keymap::RuntimeKeymap;
use crate::macos::suggest_panel::SuggestPanel;
use crate::macos::system::MacSystem;
use crate::predictor::{CommittedWord, Decision, Predictor, PredictorConfig};
use crate::spell::{self, SpellConfig, SuggestionKind};

/// Маска событий тапа: keyDown + flagsChanged.
const TAP_MASK: ffi::CGEventMask =
    (1 << ffi::KCG_EVENT_KEY_DOWN as u64) | (1 << ffi::KCG_EVENT_FLAGS_CHANGED as u64);
/// Лимит UTF-16-единиц на одно unicode-событие (поле события ограничено ~20).
const UNICODE_CHUNK: usize = 20;
/// Время жизни попапа подсказок без взаимодействия.
const SUGGEST_TIMEOUT: Duration = Duration::from_secs(8);

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
}

impl TypingPipeline {
    /// Создаёт конвейер и вешает тап на main run loop.
    /// `None` — тап создать не удалось (нет разрешения): фича выключена,
    /// остальное приложение продолжает работать.
    pub fn install(config: &Config) -> Option<Box<Self>> {
        let posting_source =
            unsafe { ffi::CGEventSourceCreate(ffi::KCG_EVENT_SOURCE_STATE_COMBINED) };
        if posting_source.is_null() {
            log::warn!("Режим печати: не удалось создать CGEventSource");
            return None;
        }

        // Конвейер сначала (колбэку нужен указатель на него как userInfo),
        // поля тапа заполняем после создания; тап не сработает раньше,
        // чем его source добавлен в run loop.
        let mut pipeline = Box::new(Self {
            predictor: Predictor::new(
                crate::predictor::LangPair::load_embedded(),
                predictor_cfg(config),
            ),
            keymap: RuntimeKeymap::build(),
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
        });
        // Клик по кнопке попапа должен попасть в конвейер: адрес аллокации
        // Box стабилен, пока конвейер жив.
        let pipe_ptr: *mut TypingPipeline = pipeline.as_mut();
        pipeline.suggest_panel.set_pipe(pipe_ptr);
        let user_info: *mut TypingPipeline = pipeline.as_mut();

        // Активный тап (может менять/поглощать события — запас на будущее;
        // сейчас события мы не модифицируем). При отказе (нет Accessibility) —
        // слушающий, чтобы хотя бы работал хоткей.
        let mut listen_only = false;
        let mut tap_port = unsafe {
            ffi::CGEventTapCreate(
                ffi::KCG_SESSION_EVENT_TAP,
                ffi::KCG_HEAD_INSERT_EVENT_TAP,
                ffi::KCG_EVENT_TAP_OPTION_DEFAULT,
                TAP_MASK,
                tap_callback,
                user_info as *mut core::ffi::c_void,
            )
        };
        if tap_port.is_null() {
            listen_only = true;
            log::warn!(
                "Режим печати: активный тап не создан (нет разрешения Accessibility?), \
                 пробуем слушающий (только хоткей)"
            );
            tap_port = unsafe {
                ffi::CGEventTapCreate(
                    ffi::KCG_SESSION_EVENT_TAP,
                    ffi::KCG_HEAD_INSERT_EVENT_TAP,
                    ffi::KCG_EVENT_TAP_OPTION_LISTEN_ONLY,
                    TAP_MASK,
                    tap_callback,
                    user_info as *mut core::ffi::c_void,
                )
            };
        }
        if tap_port.is_null() {
            log::warn!("Режим печати: тап не создан — функция отключена");
            unsafe { ffi::CFRelease(posting_source as ffi::CFTypeRef) };
            return None;
        }
        let tap_source =
            unsafe { ffi::CFMachPortCreateRunLoopSource(core::ptr::null_mut(), tap_port, 0) };
        if tap_source.is_null() {
            log::warn!("Режим печати: run-loop source не создан");
            unsafe {
                ffi::CFRelease(tap_port as ffi::CFTypeRef);
                ffi::CFRelease(posting_source as ffi::CFTypeRef);
            }
            return None;
        }
        unsafe {
            ffi::CFRunLoopAddSource(
                ffi::CFRunLoopGetMain(),
                tap_source,
                ffi::kCFRunLoopCommonModes,
            );
        }
        pipeline.tap_port = tap_port;
        pipeline.tap_source = tap_source;

        log::info!(
            "Режим печати: тап установлен ({}), typing_detect={}",
            if listen_only { "listen-only" } else { "active" },
            config.typing_detect
        );
        Some(pipeline)
    }

    /// Периодический тик из поллинга (UI NSTimer / headless CFRunLoopTimer):
    /// обновляет конфиг, исключения, поднимает упавший тап и следит за
    /// попапом подсказок (смена приложения, таймаут).
    pub fn tick(&mut self, system: &mut MacSystem, config: &Config) {
        self.typing_detect = config.typing_detect;
        self.retro_fix = config.retro_fix;
        self.hotkey = config.convert_hotkey;
        self.spell = spell_cfg(config);
        // Хоткей работает независимо от typing_detect, поэтому слова
        // отслеживаются всегда; при выключенном автопереключении решения
        // подавляем запретительно большим min_prefix.
        let mut pcfg = predictor_cfg(config);
        if !config.typing_detect {
            pcfg.min_prefix = usize::MAX;
        }
        self.predictor.update_config(pcfg);

        // Исключения: приложение из exclude_apps или мы сами (окно настроек).
        let mut active: Option<(String, String)> = None;
        self.excluded = false;
        if let Some(win) = system.active_window() {
            self.excluded = is_own_app(&win.app_id, &win.app_name, self.own_exe.as_deref())
                || config.is_excluded(&win.app_id)
                || config.is_excluded(&win.app_name);
            active = Some((win.app_id, win.app_name));
        }

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
        unsafe {
            ffi::CGEventTapEnable(self.tap_port, true);
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
            let mods = ffi::KCG_FLAG_CONTROL
                | ffi::KCG_FLAG_OPTION
                | ffi::KCG_FLAG_COMMAND
                | ffi::KCG_FLAG_NUMERIC_PAD;
            let plain_digit = self.flags & mods == 0
                && self.flags & ffi::KCG_FLAG_SHIFT == 0
                && digit_index(keycode).is_some_and(|i| i < self.pending_fixes.len());
            if plain_digit {
                let index = digit_index(keycode).expect("проверено выше");
                self.apply_suggest_fix(index);
                return core::ptr::null_mut();
            }
            self.hide_suggest_panel();
            // Событие не поглощаем — обычная печать продолжается.
        }

        // 4. Хоткей конвертации работает всегда (модификаторы сверяем точно).
        if self.hotkey_matches(keycode) {
            log::debug!("хоткей: нажатие распознано (keycode={keycode:#x})");
            self.convert_last_word();
            return event;
        }

        // Любая аппаратная клавиша «погашает» взведённый тап модификатора.
        if self.hotkey.is_mod_only() {
            self.mod_tap_armed = false;
        }

        // 5. В исключениях не вмешиваемся (и слова не запоминаем).
        // При typing_detect=false слова всё равно трекаются — для хоткея;
        // сами решения подавлены min_prefix=MAX (см. tick).
        if self.excluded {
            self.predictor.reset_word();
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
        self.panel_app = self.system.active_window().map(|w| (w.app_id, w.app_name));
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
        let Some(fix) = self.pending_fixes.get(index) else {
            return;
        };
        let fix = SuggestFix {
            backspaces: fix.backspaces,
            text: fix.text.clone(),
            layout: fix.layout,
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
                    self.convert_last_word();
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
        for chunk in units.chunks(UNICODE_CHUNK.max(1)) {
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
}

impl Drop for TypingPipeline {
    fn drop(&mut self) {
        unsafe {
            ffi::CGEventTapEnable(self.tap_port, false);
            // Снятие source с main run loop и освобождение ресурсов.
            ffi::CFRunLoopRemoveSource(
                ffi::CFRunLoopGetMain(),
                self.tap_source,
                ffi::kCFRunLoopCommonModes,
            );
            ffi::CFRelease(self.tap_source as ffi::CFTypeRef);
            ffi::CFRelease(self.tap_port as ffi::CFTypeRef);
            ffi::CFRelease(self.posting_source as ffi::CFTypeRef);
        }
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
        let mut cfg = Config::default();
        cfg.spell_suggest = false;
        cfg.spell_min_chars = 0;
        cfg.spell_max_suggestions = 42;
        let s = spell_cfg(&cfg);
        assert!(!s.enabled);
        assert_eq!(s.min_chars, 1);
        assert_eq!(s.max_suggestions, 9);
    }
}
