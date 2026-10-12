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

use crate::config::{Config, DefaultLayout};
use crate::engine::{Action, Layout, Switcher, System};
use crate::macos::event_tap::TypingPipeline;
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
}

fn layout_label(l: Layout) -> &'static str {
    match l {
        Layout::En => "EN",
        Layout::Ru => "RU",
        Layout::Other => "—",
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
                self.switcher.reload_config(cfg);
            }
        }
        if let Some(pipeline) = self.pipeline.as_mut() {
            pipeline.tick(&mut self.system, self.switcher.config());
        }
        self.refresh_ui();
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
            initWithContentRect: rect(0.0, 0.0, 420.0, 371.0)
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
        let check: Id = msg_send![check, initWithFrame: rect(20.0, 292.0, 380.0, 24.0)];
        let _: () = msg_send![check, setButtonType: 3isize]; // NSButtonTypeSwitch (чекбокс)
        let _: () = msg_send![check, setTitle: nsstring("Автокоррекция раскладки")];
        let _: () = msg_send![content, addSubview: check];

        // Чекбокс «Автопереключение по печати» (typing_detect).
        let typing_check: Id = msg_send![class!(NSButton), alloc];
        let typing_check: Id =
            msg_send![typing_check, initWithFrame: rect(20.0, 264.0, 380.0, 24.0)];
        let _: () = msg_send![typing_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            typing_check,
            setTitle: nsstring("Автопереключение по ходу печати (Punto-стиль)")
        ];
        let _: () = msg_send![content, addSubview: typing_check];

        // Чекбокс «Ретро-фикс».
        let retro_check: Id = msg_send![class!(NSButton), alloc];
        let retro_check: Id = msg_send![retro_check, initWithFrame: rect(20.0, 208.0, 380.0, 24.0)];
        let _: () = msg_send![retro_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            retro_check,
            setTitle: nsstring("Исправлять уже набранный префикс слова")
        ];
        let _: () = msg_send![content, addSubview: retro_check];

        // Чекбокс «Спеллчек-подсказки» (spell_suggest).
        let spell_check: Id = msg_send![class!(NSButton), alloc];
        let spell_check: Id = msg_send![spell_check, initWithFrame: rect(20.0, 236.0, 380.0, 24.0)];
        let _: () = msg_send![spell_check, setButtonType: 3isize]; // чекбокс
        let _: () = msg_send![
            spell_check,
            setTitle: nsstring("Подсказки исправления слов (спеллчек)")
        ];
        let _: () = msg_send![content, addSubview: spell_check];

        // «Раскладка по умолчанию».
        let l1 = make_label(rect(20.0, 180.0, 205.0, 20.0), "Раскладка по умолчанию:");
        let _: () = msg_send![content, addSubview: l1];
        let popup: Id = msg_send![class!(NSPopUpButton), alloc];
        let popup: Id = msg_send![
            popup,
            initWithFrame: rect(232.0, 176.0, 150.0, 27.0)
            pullsDown: 0i8
        ];
        for item in ["EN", "RU", "Не менять (keep)"] {
            let _: () = msg_send![popup, addItemWithTitle: nsstring(item)];
        }
        let _: () = msg_send![content, addSubview: popup];

        // «Время жизни состояния».
        let l2 = make_label(
            rect(20.0, 140.0, 205.0, 20.0),
            "Время жизни состояния, сек:",
        );
        let _: () = msg_send![content, addSubview: l2];
        let idle = make_field(rect(232.0, 137.0, 80.0, 24.0));
        let _: () = msg_send![content, addSubview: idle];

        // «Исключения».
        let l3 = make_label(
            rect(20.0, 100.0, 205.0, 20.0),
            "Исключения (через запятую):",
        );
        let _: () = msg_send![content, addSubview: l3];
        let excl = make_field(rect(232.0, 97.0, 168.0, 24.0));
        let _: () = msg_send![excl, setPlaceholderString: nsstring("Terminal, 1Password")];
        let _: () = msg_send![content, addSubview: excl];

        // Подсказка про остальные ключи режима печати.
        let l4 = make_label(
            rect(20.0, 55.0, 390.0, 36.0),
            "Хоткей конвертации и пороги — в config.txt (convert_hotkey, min_prefix_chars, switch_threshold)",
        );
        let _: () = msg_send![content, addSubview: l4];

        // Кнопка «Сохранить».
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
        let layout_item = menu_item("Раскладка: —", NIL, sel!(tick:), false);
        let _: Id = msg_send![layout_item, retain];
        let _: () = msg_send![menu, addItem: layout_item];
        let toggle_item = menu_item("Переключить на RU", controller, sel!(onToggleLayout:), true);
        let _: Id = msg_send![toggle_item, retain];
        let _: () = msg_send![menu, addItem: toggle_item];
        let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
        add_menu_action(menu, "Настройки…", controller, sel!(onOpenSettings:));
        add_menu_action(menu, "Открыть config.txt", controller, sel!(onOpenConfig:));
        add_menu_action(menu, "Открыть журнал", controller, sel!(onOpenLog:));
        let sep2: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep2];
        add_menu_action(menu, "Завершить", controller, sel!(onQuit:));
        let _: () = msg_send![item, setMenu: menu];

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
}
