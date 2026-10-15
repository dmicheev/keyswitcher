//! Попап подсказок исправления слова (NSPanel без активации).
//!
//! Панель показывается у каретки сразу после разделителя, если завершённое
//! слово похоже на опечатку. Не крадёт фокус у редактора (borderless +
//! nonactivating): печать не прерывается. Применение варианта — клик по
//! строке (target/action через контроллер с указателем на конвейер);
//! цифры 1–9 и Esc обрабатываются самим event tap (см. `event_tap.rs`).

// Макросы objc 0.2 используют cfg(feature = "cargo-clippy") — глушим точечно
// (так же, как в ui.rs и system.rs).
#![allow(unexpected_cfgs)]

use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::OnceLock;

use objc::declare::ClassDecl;
use objc::runtime::{Class, Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use crate::macos::event_tap::TypingPipeline;
use crate::macos::system::AutoReleasePool;

type Id = *mut Object;

const NIL: Id = std::ptr::null_mut();

/// Ширина панели (под «1 длинноеслово» + запас).
const WIDTH: f64 = 220.0;
/// Высота строки-кнопки.
const ROW_H: f64 = 22.0;
/// Внутренний отступ контента.
const PAD: f64 = 6.0;
/// Зазор между строкой текста и панелью.
const GAP: f64 = 4.0;
/// Минимальный отступ панели от краёв дисплея.
const EDGE: f64 = 4.0;

/// NSRect/CGRect для msg_send (совпадает с системным типом на 64-бит macOS).
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

unsafe impl objc::Encode for NsRect {
    fn encode() -> objc::Encoding {
        // Safety: строка — корректный encoding-типекод CGRect.
        unsafe { objc::Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

/// Попап-панель подсказок; живёт вместе с конвейером режима печати.
pub(crate) struct SuggestPanel {
    panel: Id,
    /// ObjC-контроллер кнопок (держит pipePtr для кликов).
    controller: Id,
    /// Текущие строки-кнопки (удерживаются супервью).
    rows: Vec<Id>,
}

impl SuggestPanel {
    /// Создаёт панель (скрытую). Только main-поток.
    #[allow(clippy::new_without_default)]
    pub(crate) fn new() -> Self {
        // Safety: построение панели на main-потоке с живым AppKit.
        // Пул обязателен: в headless-режиме (`--no-ui`) колбэки тапа
        // выполняются на «сыром» CFRunLoop, где autoreleased-объекты
        // (windowBackgroundColor и т.п.) иначе копились бы бесконечно.
        let _pool = AutoReleasePool::new();
        unsafe {
            let controller: Id = msg_send![picker_class(), new];
            let panel: Id = msg_send![class!(NSPanel), alloc];
            // Borderless(0) | NonactivatingPanel(1 << 7).
            let panel: Id = msg_send![
                panel,
                initWithContentRect: rect(0.0, 0.0, WIDTH, 60.0)
                styleMask: 128u64
                backing: 2u64 // NSBackingStoreBuffered
                defer: 0i8
            ];
            let _: () = msg_send![panel, setLevel: 3isize]; // NSFloatingWindowLevel
            let _: () = msg_send![panel, setHasShadow: 1i8];
            let _: () = msg_send![panel, setReleasedWhenClosed: 0i8];
            // Не прятать панель при деактивации нашего приложения и не забирать фокус.
            let _: () = msg_send![panel, setHidesOnDeactivate: 0i8];
            let _: () = msg_send![panel, setBecomesKeyOnlyIfNeeded: 1i8];
            let _: () = msg_send![panel, setOpaque: 0i8];
            let bg: Id = msg_send![class!(NSColor), windowBackgroundColor];
            let _: () = msg_send![panel, setBackgroundColor: bg];
            // CanJoinAllSpaces(1) | IgnoresCycle(16).
            let _: () = msg_send![panel, setCollectionBehavior: 17usize];
            Self {
                panel,
                controller,
                rows: Vec::new(),
            }
        }
    }

    /// Связывает кнопки панели с конвейером (клик → применение варианта).
    pub(crate) fn set_pipe(&mut self, pipe: *mut TypingPipeline) {
        // Safety: контроллер создан нами, запись ivar на main-потоке.
        unsafe {
            (*self.controller).set_ivar("pipePtr", pipe as *mut c_void);
        }
    }

    /// Показывает панель в точке привязки `caret` `(x, y)` — левый нижний
    /// угол каретки (координаты CG/AX). `None` — у указателя мыши
    /// (правее-ниже курсора), как раньше. Только main-поток.
    pub(crate) fn show(&mut self, labels: &[String], caret: Option<(f64, f64)>) {
        // Safety: работа с AppKit на main-потоке. Пул — как в new():
        // autoreleased NSString/шрифты не должны течь в headless-режиме.
        let _pool = AutoReleasePool::new();
        unsafe {
            self.rebuild_rows(labels);
            let height = ROW_H * labels.len().clamp(1, 9) as f64 + 2.0 * PAD;

            // Точка привязки в «экранных» координатах CG/AX (origin — левый
            // верхний угол главного монитора): каретка, а без неё — мышь.
            let (mx, my) = match caret {
                Some(p) => p,
                None => {
                    let Some(p) = crate::macos::system::ax_mouse_location() else {
                        log::warn!("попап: позиция каретки и мыши недоступна");
                        return;
                    };
                    p
                }
            };

            // Геометрия в координатах AX/CG: flip_h — высота ГЛАВНОГО
            // дисплея (перевод в AppKit всегда по нему, на каком бы
            // дисплее ни была точка), bounds — дисплея с курсором.
            // Фолбек при недоступности списка дисплеев — не показываем.
            let Some((flip_h, dx, dy, dw, dh)) = crate::macos::system::ax_display_geometry(mx, my)
            else {
                log::warn!("попап: геометрия дисплеев недоступна");
                return;
            };

            // Под точкой привязки (чуть ниже); если не влезает — над ней;
            // в любом случае не выходим за пределы дисплея с точкой.
            let mut top_quartz = my + GAP;
            if top_quartz + height > dy + dh - EDGE {
                top_quartz = my - GAP - height;
            }
            top_quartz = top_quartz.clamp(dy, (dy + dh - height).max(dy));
            let x = mx.clamp(dx, (dx + dw - WIDTH).max(dx));
            // AX/CG считает y вниз от верхнего края — AppKit вверх от нижнего.
            let top_appkit = flip_h - top_quartz;
            let frame = rect(x, top_appkit - height, WIDTH, height);
            let anchor_src = if caret.is_some() {
                "каретка"
            } else {
                "мышь"
            };
            log::debug!(
                "попап: привязка={anchor_src} ({mx:.0},{my:.0}) flip_h={flip_h:.0} \
                 дисплей=({dx:.0},{dy:.0} {dw:.0}x{dh:.0}) кадр=({:.0},{:.0}) размер={height:.0}",
                frame.x,
                frame.y
            );
            let _: () = msg_send![self.panel, setFrame: frame display: 0i8];
            let _: () = msg_send![self.panel, orderFrontRegardless];
        }
    }

    /// Прячет панель (кнопки остаются для следующего показа). Только main-поток.
    pub(crate) fn hide(&self) {
        // Safety: orderOut на main-потоке.
        unsafe {
            let _: () = msg_send![self.panel, orderOut: NIL];
        }
    }

    /// Пересобирает строки-кнопки под новый список подсказок.
    ///
    /// Safety: main-поток.
    unsafe fn rebuild_rows(&mut self, labels: &[String]) {
        self.clear_rows();
        let content: Id = msg_send![self.panel, contentView];
        let font: Id = msg_send![class!(NSFont), systemFontOfSize: 12.0f64];
        for (i, label) in labels.iter().enumerate() {
            let b: Id = msg_send![class!(NSButton), alloc];
            let b: Id = msg_send![
                b,
                initWithFrame: rect(PAD, PAD + i as f64 * ROW_H, WIDTH - 2.0 * PAD, ROW_H)
            ];
            let _: () = msg_send![b, setTitle: nsstring(label)];
            let _: () = msg_send![b, setFont: font];
            let _: () = msg_send![b, setBordered: 0i8];
            let _: () = msg_send![b, setAlignment: 0usize]; // NSTextAlignmentLeft
            let _: () = msg_send![b, setTag: i as isize];
            let _: () = msg_send![b, setTarget: self.controller];
            let _: () = msg_send![b, setAction: sel!(onPickSuggestion:)];
            let _: () = msg_send![content, addSubview: b];
            self.rows.push(b); // удерживается contentView
        }
    }

    /// Убирает и освобождает старые кнопки.
    ///
    /// Safety: main-поток; кнопки созданы через alloc и не retention'нуты нами.
    unsafe fn clear_rows(&mut self) {
        for &b in &self.rows {
            let _: () = msg_send![b, removeFromSuperview];
            let _: () = msg_send![b, release];
        }
        self.rows.clear();
    }
}

impl Drop for SuggestPanel {
    fn drop(&mut self) {
        // Safety: освобождение наших объектов на main-потоке.
        unsafe {
            self.clear_rows();
            let _: () = msg_send![self.panel, release];
            let _: () = msg_send![self.controller, release];
        }
    }
}

/// Safety: main-поток. Возвращает autoreleased NSString.
unsafe fn nsstring(s: &str) -> Id {
    let c = CString::new(s).expect("NUL в строке для NSString");
    msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
}

// ---------------------------------------------------------------------------
// Контроллер кликов (target/action кнопок) — как statePtr в ui.rs.
// ---------------------------------------------------------------------------

extern "C" fn on_pick_suggestion(this: &mut Object, _cmd: Sel, sender: Id) {
    // Safety: pipePtr установлен один раз в install() и живёт дольше панели;
    // все колбэки приходят только на main-поток.
    let pipe = unsafe {
        let ptr = *(*this).get_ivar::<*mut c_void>("pipePtr") as *mut TypingPipeline;
        if ptr.is_null() {
            return;
        }
        &mut *ptr
    };
    // Safety: тег читается у кнопки-отправителя на main-потоке.
    let tag: isize = unsafe { msg_send![sender, tag] };
    pipe.apply_suggest_fix(tag.max(0) as usize);
}

fn picker_class() -> &'static Class {
    // Класс objc не Send/Sync — храним его адрес в OnceLock<usize>
    // вместо static mut (регистрация идентична и однопоточна: main).
    static CLASS: OnceLock<usize> = OnceLock::new();
    let addr = CLASS.get_or_init(|| unsafe {
        let mut decl =
            ClassDecl::new("SuggestPickerController", class!(NSObject)).expect("class decl");
        decl.add_ivar::<*mut c_void>("pipePtr");
        decl.add_method(
            sel!(onPickSuggestion:),
            on_pick_suggestion as extern "C" fn(&mut Object, Sel, Id),
        );
        decl.register() as *const Class as usize
    });
    unsafe { &*(*addr as *const Class) }
}
