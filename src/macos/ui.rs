//! Меню-бар UI для macOS: иконка с текущей раскладкой, быстрое переключение
//! и диалог настроек.
//!
//! Основной цикл переключения переезжает сюда с «спящего» поллинга на
//! NSTimer внутри `[NSApp run]`: колбэк таймера раз в тик делает шаг
//! [`Switcher::step`] и обновляет подписи в меню. Все колбэки приходят на
//! main-поток, поэтому состояние живёт в одном `UiState` без блокировок.

// Макросы objc 0.2 (msg_send!/class!) внутри используют cfg(feature = "cargo-clippy")
// — глушим точечно (см. также src/macos/system.rs).
#![allow(unexpected_cfgs)]

use std::ffi::CString;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Instant;

use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use crate::clipboard_history::ClipboardHistory;
use crate::config::{config_dir, Config, DefaultLayout};
use crate::engine::{Action, Layout, Switcher, System};
use crate::macos::event_tap::TypingPipeline;
use crate::macos::pasteboard;
use crate::macos::system::{nsstring_to_string, AutoReleasePool, MacSystem};
use crate::{log_path, RUNNING};

type Id = *mut Object;

const NIL: Id = std::ptr::null_mut();

/// Период тика таймера (совпадает с POLL_INTERVAL головного режима).
const TICK: f64 = 0.2;

/// Период горячей перезагрузки config.txt в UI-режиме (как в headless).
const CONFIG_RELOAD_SECS: f64 = 5.0;

/// NSRect/CGRect для msg_send (структурный аргумент initWithFrame: и т.п.).
#[repr(C)]
#[derive(Clone, Copy)]
struct NsRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NsRect {
    NsRect { x, y, w, h }
}

// Кодировка типа совпадает с системным NSRect/CGRect (64-бит macOS).
unsafe impl objc::Encode for NsRect {
    fn encode() -> objc::Encoding {
        // Safety: строка — корректный encoding-типекод CGRect.
        unsafe { objc::Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

/// NSPoint/CGPoint для поста синтетического события.
#[repr(C)]
#[derive(Clone, Copy)]
struct NsPoint {
    x: f64,
    y: f64,
}

unsafe impl objc::Encode for NsPoint {
    fn encode() -> objc::Encoding {
        // Safety: корректный encoding-типекод CGPoint.
        unsafe { objc::Encoding::from_str("{CGPoint=dd}") }
    }
}

/// Ссылки на контролы окна настроек (живут, пока живёт окно).
struct SettingsUi {
    window: Id,
    auto_check: Id,
    default_popup: Id,
    idle_field: Id,
    exclude_field: Id,
    /// Чекбокс «автопереключение по печати» (typing_detect).
    typing_check: Id,
    /// Чекбокс «ретро-фикс» (retro_fix).
    retro_check: Id,
    /// Чекбокс «спеллчек-подсказки» (spell_suggest).
    spell_check: Id,
    /// Чекбокс «история буфера обмена» (clipboard_history).
    clip_check: Id,
    /// Чекбокс «писать журнал» (write_log, применяется после перезапуска).
    log_check: Id,
}

/// Всё состояние агента; трогается только из main-потока (колбэки AppKit).
struct UiState {
    system: MacSystem,
    switcher: Switcher,
    config_path: PathBuf,
    /// Конвейер режима печати (event tap). None — тап не создан (нет прав).
    pipeline: Option<Box<TypingPipeline>>,
    /// Момент последней проверки config.txt на диске.
    last_config_check: Instant,
    /// Кнопка статус-айтема (показывает EN/RU).
    status_button: Id,
    /// Пункт меню «Раскладка: EN» (индикатор, disabled).
    layout_item: Id,
    /// Пункт «Переключить на …».
    toggle_item: Id,
    /// Окно настроек (создаётся лениво).
    settings: Option<SettingsUi>,
    /// История буфера обмена (новые записи в начале).
    clip: ClipboardHistory,
    /// Последний виденный changeCount буфера (`None` — ещё не опрашивали).
    clip_count: Option<isize>,
    /// Файл истории: `~/.config/smartswitcher/clipboard_history.txt`.
    clip_path: PathBuf,
    /// Подменю «Буфер обмена» (перестраивается перед открытием).
    clip_submenu: Id,
    /// Текст, ожидающий вставки после закрытия меню.
    pending_paste: Option<String>,
}

fn layout_label(l: Layout) -> &'static str {
    match l {
        Layout::En => "EN",
        Layout::Ru => "RU",
        Layout::Other => "—",
    }
}

/// Метка пункта меню истории: время копирования и первая строка записи
/// (до ~60 символов). Время — только для показа, при вставке не используется.
fn clip_entry_label(entry: &crate::clipboard_history::ClipEntry) -> String {
    let mut label = clip_time_label(entry.at);
    if !label.is_empty() {
        label.push_str("  ");
    }
    label.push_str(&entry_label(&entry.text));
    label
}

/// Метка пункта меню истории: первая строка записи, до ~60 символов.
fn entry_label(text: &str) -> String {
    const MAX: usize = 60;
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim();
    let ellipsis = lines.next().is_some() || first.chars().count() > MAX;
    let mut label: String = first.chars().take(MAX).collect();
    if ellipsis {
        label.push('…');
    }
    label
}

/// Полный текст записи для подсказки (tooltip), ограниченный по длине.
fn tooltip_text(text: &str) -> String {
    const MAX: usize = 2000;
    if text.chars().count() > MAX {
        let truncated: String = text.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        text.to_string()
    }
}

/// Текущее unix-время в секундах (0 — если часы системы «не идут»).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Человекочитаемое время копирования: «ЧЧ:ММ» для сегодняшних записей,
/// «ДД.ММ ЧЧ:ММ» — для прошлых дней; пустая строка, если время неизвестно.
fn clip_time_label(at: u64) -> String {
    if at == 0 {
        return String::new();
    }
    // Safety: localtime_r с валидным указателем; статических данных не
    // касаемся (результат — в нашей переменной), вызов на main-потоке.
    let secs = at as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let now = now_secs() as libc::time_t;
    let mut now_tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
        libc::localtime_r(&now, &mut now_tm);
    }
    let same_day = tm.tm_year == now_tm.tm_year && tm.tm_yday == now_tm.tm_yday;
    if same_day {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        format!(
            "{:02}.{:02} {:02}:{:02}",
            tm.tm_mday,
            tm.tm_mon + 1,
            tm.tm_hour,
            tm.tm_min
        )
    }
}

impl UiState {
    /// Один шаг движка + тик конвейера печати + обновление подписей меню.
    fn tick(&mut self) {
        let action = self.switcher.step(&mut self.system);
        if !matches!(action, Action::Nothing) {
            log::info!("{:?}", action);
        }
        // Горячая перезагрузка config.txt (как в headless-режиме).
        if self.last_config_check.elapsed().as_secs_f64() > CONFIG_RELOAD_SECS {
            self.last_config_check = Instant::now();
            if self.config_path.exists() {
                let cfg = Config::load(&self.config_path);
                self.clip.set_capacity(cfg.clipboard_history_size);
                self.switcher.reload_config(cfg);
            }
        }
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.tick(&mut self.system, self.switcher.config());
        }
        self.poll_clipboard();
        self.refresh_ui();
    }

    /// Опрос буфера обмена: новое копирование (смена `changeCount`) — в историю.
    fn poll_clipboard(&mut self) {
        let cfg = self.switcher.config();
        if !cfg.clipboard_history {
            self.clip_count = None;
            return;
        }
        let Some(count) = pasteboard::change_count() else {
            return;
        };
        if self.clip_count == Some(count) {
            return;
        }
        // Буферный конвейер печати временно пишет в буфер свои промежуточные
        // значения (конвертированный текст и восстановление) — не сохраняем.
        if self
            .pipeline
            .as_ref()
            .is_some_and(|pipeline| pipeline.clip_busy())
        {
            self.clip_count = Some(count);
            return;
        }
        self.clip_count = Some(count);
        // Собственные записи (наша вставка из истории) не сохраняем.
        if self.system.frontmost_pid() == Some(i64::from(std::process::id())) {
            return;
        }
        let Some(text) = pasteboard::string() else {
            return;
        };
        // Копирования из приложений-исключений (менеджеры паролей) — мимо истории.
        let excluded = self
            .system
            .active_window()
            .is_some_and(|w| cfg.is_excluded(&w.app_name) || cfg.is_excluded(&w.app_id));
        if excluded {
            return;
        }
        if self.clip.push_at(&text, now_secs()) {
            log::debug!("История буфера: записей={}", self.clip.len());
            self.clip.save(&self.clip_path);
        }
    }

    /// Перестраивает элементы подменю истории (вызывается перед открытием).
    fn rebuild_clip_menu(&mut self, menu: Id, controller: Id) {
        // Safety: перестроение меню на main-потоке (AppKit).
        unsafe {
            let _: () = msg_send![menu, removeAllItems];
            if !self.switcher.config().clipboard_history {
                let item = menu_item(
                    "История выключена (clipboard_history)",
                    NIL,
                    sel!(tick:),
                    false,
                );
                let _: () = msg_send![menu, addItem: item];
                return;
            }
            if self.clip.is_empty() {
                let item = menu_item("(пусто)", NIL, sel!(tick:), false);
                let _: () = msg_send![menu, addItem: item];
                return;
            }
            for (idx, entry) in self.clip.entries().enumerate() {
                // Дата/время — только для показа: в буфер идёт entry.text.
                let label = clip_entry_label(entry);
                let item = menu_item(&label, controller, sel!(onPasteClipboard:), true);
                let _: () = msg_send![item, setTag: idx as isize];
                let _: () = msg_send![item, setToolTip: nsstring(&tooltip_text(&entry.text))];
                let _: () = msg_send![menu, addItem: item];
            }
            let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: sep];
            add_menu_action(
                menu,
                "Очистить историю",
                controller,
                sel!(onClearClipboard:),
            );
        }
    }

    fn refresh_ui(&mut self) {
        let layout = self.system.current_layout();
        // Safety: подписи — autoreleased NSString внутри пула run loop AppKit.
        unsafe {
            let _: () = msg_send![self.status_button, setTitle: nsstring(layout_label(layout))];
            let _: () = msg_send![
                self.layout_item,
                setTitle: nsstring(&format!("Раскладка: {}", layout_label(layout)))
            ];
            let toggle_target = match layout {
                Layout::Ru => "EN",
                _ => "RU",
            };
            let _: () = msg_send![
                self.toggle_item,
                setTitle: nsstring(&format!("Переключить на {toggle_target}"))
            ];
        }
    }
}

// ---------------------------------------------------------------------------
// Колбэки ObjC (target/action и NSTimer). `this` — контроллер,
// в ivar `statePtr` которого лежит `*mut UiState`.
// ---------------------------------------------------------------------------

fn state_of(this: *mut Object) -> &'static mut UiState {
    // Safety: указатель установлен один раз в run_ui и живёт до конца run;
    // все колбэки приходят только на main-поток.
    unsafe {
        let ptr = *(*this).get_ivar::<*mut c_void>("statePtr");
        &mut *(ptr as *mut UiState)
    }
}

extern "C" fn on_tick(this: &mut Object, _cmd: Sel, _timer: Id) {
    state_of(this as *mut Object).tick();
    // Уважаем SIGINT/SIGTERM: выходим из NSApp.run.
    if !RUNNING.load(Ordering::SeqCst) {
        stop_run_loop();
    }
}

extern "C" fn on_toggle_layout(this: &mut Object, _cmd: Sel, _sender: Id) {
    let state = state_of(this as *mut Object);
    let target = match state.system.current_layout() {
        Layout::Ru => Layout::En,
        _ => Layout::Ru,
    };
    if !state.system.set_layout(target) {
        log::warn!("Не удалось переключить раскладку на {:?}", target);
    }
    // Движок на следующем тике сам заметит смену как «ручную» и запомнит её.
    state.refresh_ui();
}

extern "C" fn on_open_settings(this: &mut Object, _cmd: Sel, _sender: Id) {
    let controller = this as Id;
    let state = state_of(this as *mut Object);
    if state.settings.is_none() {
        state.settings = Some(build_settings(controller));
    }
    let Some(s) = state.settings.as_ref() else {
        return;
    };
    fill_settings(s, state.switcher.config());
    // Safety: показ окна и активация приложения.
    unsafe {
        let _: () = msg_send![s.window, makeKeyAndOrderFront: NIL];
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, activateIgnoringOtherApps: 1i8];
    }
}

extern "C" fn on_save_settings(this: &mut Object, _cmd: Sel, _sender: Id) {
    let state = state_of(this as *mut Object);
    let Some(s) = state.settings.as_ref() else {
        return;
    };
    let cfg = read_settings(s, state.switcher.config());
    if let Err(e) = cfg.save(&state.config_path) {
        log::error!("Не удалось сохранить конфигурацию: {e}");
        return;
    }
    log::info!("Конфигурация сохранена (default={:?})", cfg.default_layout);
    state.switcher.reload_config(cfg);
    // Safety: прячем окно (не разрушаем — следующий откроется мгновенно).
    unsafe {
        let _: () = msg_send![s.window, orderOut: NIL];
    }
}

extern "C" fn on_open_config(this: &mut Object, _cmd: Sel, _sender: Id) {
    open_in_default_app(&state_of(this as *mut Object).config_path);
}

extern "C" fn on_open_log(_this: &mut Object, _cmd: Sel, _sender: Id) {
    open_in_default_app(&log_path());
}

extern "C" fn on_quit(_this: &mut Object, _cmd: Sel, _sender: Id) {
    stop_run_loop();
}

/// Делегат NSMenu: перестроение подменю истории перед его открытием.
extern "C" fn on_menu_needs_update(this: &mut Object, _cmd: Sel, menu: Id) {
    let controller = this as Id;
    let state = state_of(this as *mut Object);
    if menu == state.clip_submenu {
        state.rebuild_clip_menu(menu, controller);
    }
}

/// Клик по записи истории: вернуть её в буфер и сразу вставить в активное
/// приложение. Вставляется только текст записи (время — элемент показа).
/// Сама вставка — с задержкой, чтобы завершилась анимация меню.
extern "C" fn on_paste_clipboard(this: &mut Object, _cmd: Sel, sender: Id) {
    let state = state_of(this as *mut Object);
    let tag: isize = unsafe { msg_send![sender, tag] };
    let Some(entry) = (tag >= 0)
        .then(|| state.clip.get(tag as usize))
        .flatten()
        .map(|e| e.text.clone())
    else {
        return;
    };
    // Запись кладём в буфер и поднимаем наверх истории со свежим временем;
    // свой же пост в историю не попадает (changeCount зафиксирован здесь же).
    if pasteboard::set_string(&entry) {
        state.clip_count = pasteboard::change_count();
        state.clip.push_at(&entry, now_secs());
        state.clip.save(&state.clip_path);
    }
    state.pending_paste = Some(entry);
    // Safety: performSelector:afterDelay: планирует вызов в default mode
    // run loop — сработает после закрытия меню.
    unsafe {
        let _: () = msg_send![
            this as Id,
            performSelector: sel!(onPastePending:)
            withObject: NIL
            afterDelay: 0.1
        ];
    }
}

/// Отложенная вставка (Cmd+V) записи истории в активное приложение.
extern "C" fn on_paste_pending(this: &mut Object, _cmd: Sel, _sender: Id) {
    let state = state_of(this as *mut Object);
    if state.pending_paste.take().is_some() {
        pasteboard::paste_cmd_v();
    }
}

/// Очищает историю копирований (память и файл).
extern "C" fn on_clear_clipboard(this: &mut Object, _cmd: Sel, _sender: Id) {
    let state = state_of(this as *mut Object);
    state.clip.clear();
    state.clip.save(&state.clip_path);
    log::info!("История буфера обмена очищена");
}

fn stop_run_loop() {
    // Safety: обычные вызовы AppKit на main-потоке.
    unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, stop: NIL];
        // stop: срабатывает только при обработке «настоящего» события:
        // колбэк таймера таковым не считается. Будим run loop синтетическим
        // событием (классический приём для menu-bar агентов).
        let event: Id = msg_send![
            class!(NSEvent),
            otherEventWithType: 15isize // NSEventTypeApplicationDefined
            location: NsPoint { x: 0.0, y: 0.0 }
            modifierFlags: 0usize
            timestamp: 0.0f64
            windowNumber: 0isize
            context: NIL
            subtype: 0i16
            data1: 0isize
            data2: 0isize
        ];
        let _: () = msg_send![app, postEvent: event atStart: 1i8];
    }
}

// ---------------------------------------------------------------------------
// Построение UI. Все unsafe-функции вызываются только на main-потоке.
// ---------------------------------------------------------------------------

/// Safety: main-поток. Возвращает autoreleased NSString.
unsafe fn nsstring(s: &str) -> Id {
    let c = CString::new(s).expect("NUL в строке для NSString");
    msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
}

/// Safety: main-поток. `target == NIL` — неактивный пункт-индикатор.
unsafe fn menu_item(title: &str, target: Id, action: Sel, enabled: bool) -> Id {
    let item: Id = msg_send![class!(NSMenuItem), alloc];
    let item: Id = msg_send![
        item,
        initWithTitle: nsstring(title)
        action: action
        keyEquivalent: nsstring("")
    ];
    if target != NIL {
        let _: () = msg_send![item, setTarget: target];
    }
    let _: () = msg_send![item, setEnabled: enabled as i8];
    item
}

fn build_settings(controller: Id) -> SettingsUi {
    // Safety: построение окна на main-потоке.
    unsafe {
        let window: Id = msg_send![class!(NSWindow), alloc];
        // Titled(1) | Closable(2), Buffered(2), defer = NO.
        let window: Id = msg_send![
            window,
            initWithContentRect: rect(0.0, 0.0, 420.0, 405.0)
            styleMask: 3u64
            backing: 2u64
            defer: 0i8
        ];
        let _: () = msg_send![window, setTitle: nsstring("SmartSwitcher — настройки")];
        // Окно держим сами: закрытие = orderOut, без release.
        let _: () = msg_send![window, setReleasedWhenClosed: 0i8];
        let content: Id = msg_send![window, contentView];

        // Чекбокс «Автокоррекция».
        let check: Id = msg_send![class!(NSButton), alloc];
        let check: Id = msg_send![check, initWithFrame: rect(20.0, 322.0, 380.0, 24.0)];
        let _: () = msg_send![check, setButtonType: 3isize]; // NSButtonTypeSwitch (чекбокс)
        let _: () = msg_send![check, setTitle: nsstring("Автокоррекция раскладки")];
        let _: () = msg_send![content, addSubview: check];

        // Чекбокс «Автопереключение по печати» (typing_detect).
        let typing_check: Id = msg_send![class!(NSButton), alloc];
        let typing_check: Id =
            msg_send![typing_check, initWithFrame: rect(20.0, 294.0, 380.0, 24.0)];
        let _: () = msg_send![typing_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            typing_check,
            setTitle: nsstring("Автопереключение по ходу печати (Punto-стиль)")
        ];
        let _: () = msg_send![content, addSubview: typing_check];

        // Чекбокс «Спеллчек-подсказки» (spell_suggest).
        let spell_check: Id = msg_send![class!(NSButton), alloc];
        let spell_check: Id = msg_send![spell_check, initWithFrame: rect(20.0, 266.0, 380.0, 24.0)];
        let _: () = msg_send![spell_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            spell_check,
            setTitle: nsstring("Подсказки исправления слов (спеллчек)")
        ];
        let _: () = msg_send![content, addSubview: spell_check];

        // Чекбокс «Ретро-фикс».
        let retro_check: Id = msg_send![class!(NSButton), alloc];
        let retro_check: Id = msg_send![retro_check, initWithFrame: rect(20.0, 238.0, 380.0, 24.0)];
        let _: () = msg_send![retro_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            retro_check,
            setTitle: nsstring("Исправлять уже набранный префикс слова")
        ];
        let _: () = msg_send![content, addSubview: retro_check];

        // Чекбокс «История буфера обмена» (clipboard_history).
        let clip_check: Id = msg_send![class!(NSButton), alloc];
        let clip_check: Id = msg_send![clip_check, initWithFrame: rect(20.0, 210.0, 380.0, 24.0)];
        let _: () = msg_send![clip_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            clip_check,
            setTitle: nsstring("История буфера обмена (меню «Буфер обмена»)")
        ];
        let _: () = msg_send![content, addSubview: clip_check];

        // Чекбокс «Писать журнал» (write_log, применяется после перезапуска).
        let log_check: Id = msg_send![class!(NSButton), alloc];
        let log_check: Id = msg_send![log_check, initWithFrame: rect(20.0, 182.0, 380.0, 24.0)];
        let _: () = msg_send![log_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            log_check,
            setTitle: nsstring("Писать журнал (применится после перезапуска)")
        ];
        let _: () = msg_send![content, addSubview: log_check];

        // «Раскладка по умолчанию».
        let l1 = make_label(rect(20.0, 148.0, 205.0, 20.0), "Раскладка по умолчанию:");
        let _: () = msg_send![content, addSubview: l1];
        let popup: Id = msg_send![class!(NSPopUpButton), alloc];
        let popup: Id = msg_send![
            popup,
            initWithFrame: rect(232.0, 144.0, 150.0, 27.0)
            pullsDown: 0i8
        ];
        for item in ["EN", "RU", "Не менять (keep)"] {
            let _: () = msg_send![popup, addItemWithTitle: nsstring(item)];
        }
        let _: () = msg_send![content, addSubview: popup];

        // «Время жизни состояния».
        let l2 = make_label(
            rect(20.0, 108.0, 205.0, 20.0),
            "Время жизни состояния, сек:",
        );
        let _: () = msg_send![content, addSubview: l2];
        let idle = make_field(rect(232.0, 105.0, 80.0, 24.0));
        let _: () = msg_send![content, addSubview: idle];

        // «Исключения».
        let l3 = make_label(rect(20.0, 68.0, 205.0, 20.0), "Исключения (через запятую):");
        let _: () = msg_send![content, addSubview: l3];
        let excl = make_field(rect(232.0, 65.0, 168.0, 24.0));
        let _: () = msg_send![excl, setPlaceholderString: nsstring("Terminal, 1Password")];
        let _: () = msg_send![content, addSubview: excl];

        // Кнопка «Сохранить» (рядом — подсказка про ключи в config.txt).
        let l4 = make_label(
            rect(20.0, 22.0, 290.0, 18.0),
            "Хоткей и пороги печати — в config.txt",
        );
        let _: () = msg_send![content, addSubview: l4];
        let save: Id = msg_send![class!(NSButton), alloc];
        let save: Id = msg_send![save, initWithFrame: rect(320.0, 15.0, 80.0, 32.0)];
        let _: () = msg_send![save, setTitle: nsstring("Сохранить")];
        let _: () = msg_send![save, setBezelStyle: 1u64]; // NSRoundedBezelStyle
        let _: () = msg_send![save, setTarget: controller];
        let _: () = msg_send![save, setAction: sel!(onSaveSettings:)];
        let _: () = msg_send![content, addSubview: save];

        SettingsUi {
            window,
            auto_check: check,
            default_popup: popup,
            idle_field: idle,
            exclude_field: excl,
            typing_check,
            retro_check,
            spell_check,
            clip_check,
            log_check,
        }
    }
}

/// Safety: main-поток.
unsafe fn make_label(frame: NsRect, text: &str) -> Id {
    let field: Id = msg_send![class!(NSTextField), alloc];
    let field: Id = msg_send![field, initWithFrame: frame];
    let _: () = msg_send![field, setStringValue: nsstring(text)];
    let _: () = msg_send![field, setEditable: 0i8];
    let _: () = msg_send![field, setSelectable: 0i8];
    let _: () = msg_send![field, setBezeled: 0i8];
    let _: () = msg_send![field, setDrawsBackground: 0i8];
    field
}

/// Safety: main-поток.
unsafe fn make_field(frame: NsRect) -> Id {
    let field: Id = msg_send![class!(NSTextField), alloc];
    let field: Id = msg_send![field, initWithFrame: frame];
    let _: () = msg_send![field, setBezeled: 1i8];
    field
}

fn fill_settings(s: &SettingsUi, cfg: &Config) {
    // Safety: запись значений контролов на main-потоке.
    unsafe {
        let _: () = msg_send![s.auto_check, setState: cfg.auto_correct as isize];
        let _: () = msg_send![s.typing_check, setState: cfg.typing_detect as isize];
        let _: () = msg_send![s.retro_check, setState: cfg.retro_fix as isize];
        let _: () = msg_send![s.spell_check, setState: cfg.spell_suggest as isize];
        let _: () = msg_send![s.clip_check, setState: cfg.clipboard_history as isize];
        let _: () = msg_send![s.log_check, setState: cfg.write_log as isize];
        let idx = match cfg.default_layout {
            DefaultLayout::En => 0,
            DefaultLayout::Ru => 1,
            DefaultLayout::Keep => 2,
        };
        let _: () = msg_send![s.default_popup, selectItemAtIndex: idx];
        let _: () = msg_send![
            s.idle_field,
            setStringValue: nsstring(&cfg.idle_pause_seconds.to_string())
        ];
        let mut apps: Vec<String> = cfg.excluded_apps.iter().cloned().collect();
        apps.sort();
        let _: () = msg_send![s.exclude_field, setStringValue: nsstring(&apps.join(", "))];
    }
}

/// Читает настройки из диалога. Ключи, не представленные в UI
/// (min_prefix_chars и т.п.), наследуются из `base`.
fn read_settings(s: &SettingsUi, base: &Config) -> Config {
    // Safety: чтение значений контролов на main-потоке.
    unsafe {
        let on: isize = msg_send![s.auto_check, state];
        let idx: isize = msg_send![s.default_popup, indexOfSelectedItem];
        let idle: isize = msg_send![s.idle_field, integerValue];
        let excl: Id = msg_send![s.exclude_field, stringValue];
        let excl = nsstring_to_string(excl).unwrap_or_default();
        let typing: isize = msg_send![s.typing_check, state];
        let retro: isize = msg_send![s.retro_check, state];
        let spell: isize = msg_send![s.spell_check, state];
        let clip_history: isize = msg_send![s.clip_check, state];
        let write_log: isize = msg_send![s.log_check, state];

        Config {
            auto_correct: on == 1,
            default_layout: match idx {
                0 => DefaultLayout::En,
                1 => DefaultLayout::Ru,
                _ => DefaultLayout::Keep,
            },
            idle_pause_seconds: idle.max(1) as u64,
            excluded_apps: excl
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            typing_detect: typing == 1,
            retro_fix: retro == 1,
            spell_suggest: spell == 1,
            clipboard_history: clip_history == 1,
            write_log: write_log == 1,
            ..base.clone()
        }
    }
}

fn open_in_default_app(path: &Path) {
    // Safety: NSWorkspace openURL:.
    unsafe {
        let url: Id = msg_send![
            class!(NSURL),
            fileURLWithPath: nsstring(&path.display().to_string())
        ];
        let ws: Id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let _: () = msg_send![ws, openURL: url];
    }
}

// ---------------------------------------------------------------------------
// Регистрация ObjC-класса контроллера
// ---------------------------------------------------------------------------

fn controller_class() -> &'static Class {
    // Класс objc не Send/Sync — храним адрес в OnceLock<usize> вместо
    // static mut (регистрация однопоточна: main).
    static CLASS: OnceLock<usize> = OnceLock::new();
    let addr = CLASS.get_or_init(|| unsafe {
        let mut decl =
            ClassDecl::new("SmartSwitcherController", class!(NSObject)).expect("class decl");
        decl.add_ivar::<*mut c_void>("statePtr");
        decl.add_method(sel!(tick:), on_tick as extern "C" fn(&mut Object, Sel, Id));
        decl.add_method(
            sel!(onToggleLayout:),
            on_toggle_layout as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onOpenSettings:),
            on_open_settings as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onSaveSettings:),
            on_save_settings as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onOpenConfig:),
            on_open_config as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onOpenLog:),
            on_open_log as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onQuit:),
            on_quit as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(menuNeedsUpdate:),
            on_menu_needs_update as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onPasteClipboard:),
            on_paste_clipboard as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onClearClipboard:),
            on_clear_clipboard as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.add_method(
            sel!(onPastePending:),
            on_paste_pending as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.register() as *const Class as usize
    });
    unsafe { &*(*addr as *const Class) }
}

// ---------------------------------------------------------------------------
// Запуск
// ---------------------------------------------------------------------------

/// Запускает агент с UI: статус-айтем, меню, настройки и цикл на NSTimer.
/// Блокирующий вызов (`NSApp.run`) до выхода (пункт «Завершить», SIGINT/SIGTERM).
pub fn run_ui(system: MacSystem, config: Config) {
    unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        // NSApplicationActivationPolicyAccessory = 1: агент без иконки в Dock.
        let _: () = msg_send![app, setActivationPolicy: 1isize];

        let _pool = AutoReleasePool::new();
        let controller: Id = msg_send![controller_class(), new];

        // Статус-айтем: иконка + текущая раскладка текстом.
        let bar: Id = msg_send![class!(NSStatusBar), systemStatusBar];
        // NSVariableStatusItemLength = -1 (ширина по содержимому).
        let item: Id = msg_send![bar, statusItemWithLength: -1.0f64];
        let button: Id = msg_send![item, button];
        let _: Id = msg_send![button, retain];
        let img: Id = msg_send![
            class!(NSImage),
            imageWithSystemSymbolName: nsstring("keyboard")
            accessibilityDescription: NIL
        ];
        if img != NIL {
            let _: () = msg_send![img, setTemplate: 1i8];
            let _: () = msg_send![button, setImage: img];
        }

        // Меню.
        let menu: Id = msg_send![class!(NSMenu), new];
        // Доступность пунктов задаём вручную: с включённым по умолчанию
        // autoenablesItems NSMenu сам дизейблит пункты без target
        // («Буфер обмена», индикатор раскладки) по responder chain.
        let _: () = msg_send![menu, setAutoenablesItems: 0i8];
        let layout_item = menu_item("Раскладка: —", NIL, sel!(tick:), false);
        let _: Id = msg_send![layout_item, retain];
        let _: () = msg_send![menu, addItem: layout_item];
        let toggle_item = menu_item("Переключить на RU", controller, sel!(onToggleLayout:), true);
        let _: Id = msg_send![toggle_item, retain];
        let _: () = msg_send![menu, addItem: toggle_item];
        let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];

        // Подменю истории буфера обмена: элементы достраиваются перед
        // открытием (делегат menuNeedsUpdate:).
        let clip_item = menu_item("Буфер обмена", NIL, sel!(tick:), true);
        let clip_submenu: Id = msg_send![class!(NSMenu), new];
        let _: () = msg_send![clip_submenu, setAutoenablesItems: 0i8];
        let _: () = msg_send![clip_submenu, setDelegate: controller];
        // Именно setSubmenu: (современное свойство); старый setSubMenu:
        // удалён из AppKit и падает doesNotRecognizeSelector.
        let _: () = msg_send![clip_item, setSubmenu: clip_submenu];
        let _: () = msg_send![menu, addItem: clip_item];

        let sep2: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep2];
        add_menu_action(menu, "Настройки…", controller, sel!(onOpenSettings:));
        add_menu_action(menu, "Открыть config.txt", controller, sel!(onOpenConfig:));
        add_menu_action(menu, "Открыть журнал", controller, sel!(onOpenLog:));
        let sep3: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep3];
        add_menu_action(menu, "Завершить", controller, sel!(onQuit:));
        let _: () = msg_send![item, setMenu: menu];

        // История буфера обмена: догружаем с диска (файл рядом с config.txt).
        let mut clip = ClipboardHistory::new(config.clipboard_history_size);
        let clip_path = config_dir().join("clipboard_history.txt");
        clip.load(&clip_path);

        let mut state = UiState {
            system,
            switcher: Switcher::new(config.clone()),
            config_path: Config::default_path(),
            pipeline: TypingPipeline::install(&config),
            last_config_check: Instant::now(),
            status_button: button,
            layout_item,
            toggle_item,
            settings: None,
            clip,
            clip_count: None,
            clip_path,
            clip_submenu,
            pending_paste: None,
        };
        state.refresh_ui();
        let state_ptr = Box::into_raw(Box::new(state));
        (*controller).set_ivar("statePtr", state_ptr as *mut c_void);

        // Отладка: SMARTSWITCHER_DEBUG_SETTINGS=1 — открыть настройки на старте.
        if std::env::var("SMARTSWITCHER_DEBUG_SETTINGS").is_ok() {
            let state = &mut *state_ptr;
            if state.settings.is_none() {
                state.settings = Some(build_settings(controller));
            }
            if let Some(s) = state.settings.as_ref() {
                // Внутри уже unsafe-блока run_ui.
                let _: () = msg_send![s.window, makeKeyAndOrderFront: NIL];
                let app: Id = msg_send![class!(NSApplication), sharedApplication];
                let _: () = msg_send![app, activateIgnoringOtherApps: 1i8];
            }
        }

        // Цикл переключения на NSTimer.
        let timer: Id = msg_send![
            class!(NSTimer),
            scheduledTimerWithTimeInterval: TICK
            target: controller
            selector: sel!(tick:)
            userInfo: NIL
            repeats: 1i8
        ];

        let _: () = msg_send![app, run];

        // Cleanup.
        let _: () = msg_send![timer, invalidate];
        drop(Box::from_raw(state_ptr));
    }
    log::info!("Остановка.");
}

/// Safety: main-поток.
unsafe fn add_menu_action(menu: Id, title: &str, controller: Id, action: Sel) {
    let item = menu_item(title, controller, action, true);
    let _: () = msg_send![menu, addItem: item];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_labels() {
        assert_eq!(layout_label(Layout::En), "EN");
        assert_eq!(layout_label(Layout::Ru), "RU");
        assert_eq!(layout_label(Layout::Other), "—");
    }

    #[test]
    fn entry_labels_are_truncated_and_single_line() {
        assert_eq!(entry_label("просто текст"), "просто текст");
        assert_eq!(entry_label("первая\nвторая"), "первая…");
        let long = "ж".repeat(80);
        let label = entry_label(&long);
        assert_eq!(label.chars().count(), 61);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn tooltip_is_bounded() {
        assert_eq!(tooltip_text("abc"), "abc");
        let long = "x".repeat(3000);
        let tip = tooltip_text(&long);
        assert_eq!(tip.chars().count(), 2001);
        assert!(tip.ends_with('…'));
    }

    #[test]
    fn clip_time_labels() {
        // Без времени — пустая метка (в меню только текст).
        assert_eq!(clip_time_label(0), "");
        // Сегодняшняя запись — «ЧЧ:ММ».
        let today = clip_time_label(now_secs());
        assert_eq!(today.chars().count(), 5, "ожидался формат ЧЧ:ММ: {today}");
        assert!(today.contains(':'));
        // Двухдневная давность — «ДД.ММ ЧЧ:ММ» (11 символов).
        let old = clip_time_label(now_secs() - 2 * 24 * 3600);
        assert_eq!(
            old.chars().count(),
            11,
            "ожидался формат ДД.ММ ЧЧ:ММ: {old}"
        );
    }

    #[test]
    fn clip_entry_label_shows_time_but_paste_uses_text_only() {
        use crate::clipboard_history::ClipEntry;
        let now = now_secs();
        let entry = ClipEntry {
            text: "только текст".into(),
            at: now,
        };
        let label = clip_entry_label(&entry);
        assert!(label.ends_with("только текст"));
        assert!(label.contains(':'), "в метке должно быть время: {label}");
        // Сама вставка берёт только text — это контракт get()/onPasteClipboard.
        assert_eq!(entry.text, "только текст");
    }
}
