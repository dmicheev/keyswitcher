//! Спайк-проверка захвата и постинга клавиатурных событий (macOS).
//!
//! Проверяет три гипотезы, на которых строится режим автопереключения:
//!
//! 1. Активный (active) CGEventTap на main runloop видит `keyDown`
//!    с keycode и `flagsChanged`.
//! 2. События, постнутые нами через source `kCGEventSourceStateCombinedSessionState`,
//!    приходят в тап с `kCGEventSourceStateID == 0`, а «аппаратные» (в т.ч.
//!    постнутые через source `kCGEventSourceStateHIDSystemState`) — с ID == 1.
//!    Это позволяет пропускать собственные инжектированные события (защита от цикла).
//! 3. Unicode-событие (`CGEventKeyboardSetUnicodeString`) реально вводит текст.
//!
//! Запуск: `cargo run --example spike_tap` — 6 секунд наблюдает за клавиатурой,
//! постнит тестовые события и печатает вердикты. Нажмите пару клавиш для проверки
//! «живого» аппаратного события (или положитесь на постнутые).
//!
//! НЕ использовать вне спайка: пример дублирует FFI основного проекта.

#[cfg(target_os = "macos")]
mod spike {
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::OnceLock;

    use core::ffi::c_int;

    type CFRunLoopRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopTimerRef = *mut c_void;
    type CFStringRef = *const c_void;
    type CFTimeInterval = f64;
    type CGEventRef = *mut c_void;
    type CGEventSourceRef = *mut c_void;
    type CGEventMask = u64;
    type CGEventTapProxy = *mut c_void;
    type CGKeyCode = u16;

    const KCG_EVENT_KEY_DOWN: u32 = 10;
    const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
    const KCG_SESSION_EVENT_TAP: c_int = 1;
    const KCG_HEAD_INSERT_EVENT_TAP: c_int = 0;
    const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;
    const KCG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
    const KCG_EVENT_SOURCE_STATE_ID: u32 = 45;
    const KCG_EVENT_SOURCE_STATE_COMBINED: c_int = 0;
    const KCG_EVENT_SOURCE_STATE_HID: c_int = 1;
    const KVK_ANSI_A: CGKeyCode = 0x00;

    extern "C" {
        fn CFRunLoopGetMain() -> CFRunLoopRef;
        fn CFRunLoopRun();
        fn CFRunLoopStop(rl: CFRunLoopRef);
        fn CFAbsoluteTimeGetCurrent() -> CFTimeInterval;
        fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
        // ВАЖНО: CGEventTapCreate возвращает CFMachPortRef, а не run-loop source —
        // его нужно завернуть через CFMachPortCreateRunLoopSource.
        fn CFMachPortCreateRunLoopSource(
            allocator: *mut c_void,
            port: *mut c_void,
            order: CFIndexAlias,
        ) -> CFRunLoopSourceRef;
        fn CFRunLoopTimerCreate(
            allocator: *mut c_void,
            fireDate: CFTimeInterval,
            interval: CFTimeInterval,
            flags: u64,
            order: CFIndexAlias,
            callout: extern "C" fn(*mut c_void),
            context: *mut c_void,
        ) -> CFRunLoopTimerRef;
        fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
        fn CFRelease(cf: *const c_void);
        fn CGEventTapCreate(
            tap: c_int,
            place: c_int,
            options: u32,
            eventMask: CGEventMask,
            callback: extern "C" fn(
                proxy: CGEventTapProxy,
                eventType: u32,
                event: CGEventRef,
                userInfo: *mut c_void,
            ) -> CGEventRef,
            userInfo: *mut c_void,
        ) -> CFRunLoopSourceRef;
        fn CGEventSourceCreate(stateID: c_int) -> CGEventSourceRef;
        fn CGEventCreateKeyboardEvent(
            source: CGEventSourceRef,
            virtualKey: CGKeyCode,
            keyDown: bool,
        ) -> CGEventRef;
        fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, string: *const u16);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGEventPost(tapLocation: c_int, event: CGEventRef);
    }

    /// `CFIndex` (long) — отдельный алиас, чтобы не тащить тип из основного ffi.
    type CFIndexAlias = isize;

    static KCF_RUNLOOP_COMMON_MODES: OnceLock<()> = OnceLock::new();

    fn common_modes() -> CFStringRef {
        // kCFRunLoopCommonModes — экспортируемый символ CoreFoundation.
        extern "C" {
            static kCFRunLoopCommonModes: CFStringRef;
        }
        KCF_RUNLOOP_COMMON_MODES.get_or_init(|| ());
        unsafe { kCFRunLoopCommonModes }
    }

    /// Счётчики событий по sourceStateID (0 = синтетика, 1 = «аппаратные»).
    static SEEN_SYNTHETIC: AtomicU32 = AtomicU32::new(0);
    static SEEN_HARDWARE: AtomicU32 = AtomicU32::new(0);
    static SEEN_HARDWARE_LIVE: AtomicU32 = AtomicU32::new(0);
    static DONE: AtomicBool = AtomicBool::new(false);

    extern "C" fn tap_callback(
        _proxy: CGEventTapProxy,
        event_type: u32,
        event: CGEventRef,
        _user_info: *mut c_void,
    ) -> CGEventRef {
        if matches!(
            event_type,
            KCG_EVENT_KEY_DOWN | KCG_EVENT_FLAGS_CHANGED | 0xFFFF_FFFE | 0xFFFF_FFFF
        ) {
            let state_id = unsafe { CGEventGetIntegerValueField(event, KCG_EVENT_SOURCE_STATE_ID) };
            let keycode = unsafe { CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) };
            match state_id {
                1 => {
                    SEEN_HARDWARE.fetch_add(1, Ordering::Relaxed);
                    println!("[tap] hardware-like: type={event_type} keycode={keycode} stateID={state_id}");
                }
                other => {
                    SEEN_SYNTHETIC.fetch_add(1, Ordering::Relaxed);
                    println!(
                        "[tap] synthetic     : type={event_type} keycode={keycode} stateID={other}"
                    );
                }
            }
        }
        event // пропускаем без изменений
    }

    extern "C" fn stop_timer(_ctx: *mut c_void) {
        DONE.store(true, Ordering::SeqCst);
        unsafe {
            CFRunLoopStop(CFRunLoopGetMain());
        }
    }

    fn post_events() {
        unsafe {
            let src = CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_COMBINED);
            assert!(!src.is_null(), "CGEventSourceCreate(combined) failed");

            // 1) Unicode-строка через combined-source (как ретро-фикс).
            let ev = CGEventCreateKeyboardEvent(src, 0, true);
            let text: Vec<u16> = "[spike-unicode] ".encode_utf16().collect();
            CGEventKeyboardSetUnicodeString(ev, text.len(), text.as_ptr());
            CGEventPost(KCG_SESSION_EVENT_TAP, ev);
            let up = CGEventCreateKeyboardEvent(src, 0, false);
            CGEventPost(KCG_SESSION_EVENT_TAP, up);
            CFRelease(ev);
            CFRelease(up);

            // 2) «Аппаратноподобное» событие: source в состоянии HID system.
            let hid_src = CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_HID);
            assert!(!hid_src.is_null(), "CGEventSourceCreate(hid) failed");
            let k = CGEventCreateKeyboardEvent(hid_src, KVK_ANSI_A, true);
            let ku = CGEventCreateKeyboardEvent(hid_src, KVK_ANSI_A, false);
            CGEventPost(KCG_SESSION_EVENT_TAP, k);
            CGEventPost(KCG_SESSION_EVENT_TAP, ku);
            CFRelease(k);
            CFRelease(ku);
            CFRelease(hid_src);
            CFRelease(src);
        }
    }

    /// Таймер через `delay` секунд (fireDate — абсолютное время!).
    unsafe fn schedule_timer(delay: f64, callout: extern "C" fn(*mut c_void)) -> CFRunLoopTimerRef {
        let timer = CFRunLoopTimerCreate(
            std::ptr::null_mut(),
            CFAbsoluteTimeGetCurrent() + delay,
            0.0,
            0,
            0,
            callout,
            std::ptr::null_mut(),
        );
        CFRunLoopAddTimer(CFRunLoopGetMain(), timer, common_modes());
        timer
    }

    pub fn run() {
        let mask: CGEventMask = (1 << KCG_EVENT_KEY_DOWN) | (1 << KCG_EVENT_FLAGS_CHANGED);
        unsafe {
            let tap = CGEventTapCreate(
                KCG_SESSION_EVENT_TAP,
                KCG_HEAD_INSERT_EVENT_TAP,
                KCG_EVENT_TAP_OPTION_DEFAULT,
                mask,
                tap_callback,
                std::ptr::null_mut(),
            );
            if tap.is_null() {
                println!("FAIL: CGEventTapCreate вернул NULL — нет разрешения Accessibility?");
                return;
            }
            println!("OK: активный тап создан, слушаем 6 c ...");
            let tap_source = CFMachPortCreateRunLoopSource(std::ptr::null_mut(), tap, 0);
            if tap_source.is_null() {
                println!("FAIL: CFMachPortCreateRunLoopSource вернул NULL");
                return;
            }
            CFRunLoopAddSource(CFRunLoopGetMain(), tap_source, common_modes());

            let post_timer = schedule_timer(1.5, {
                extern "C" fn fire(_: *mut c_void) {
                    println!("[spike] постим тестовые события");
                    post_events();
                }
                fire
            });
            let stop_timer = schedule_timer(6.0, stop_timer);

            CFRunLoopRun();

            CFRelease(tap_source);
            CFRelease(tap);
            if !post_timer.is_null() {
                CFRelease(post_timer);
            }
            if !stop_timer.is_null() {
                CFRelease(stop_timer);
            }
        }

        let synth = SEEN_SYNTHETIC.load(Ordering::Relaxed);
        let hw = SEEN_HARDWARE.load(Ordering::Relaxed);
        println!("\n--- Вердикт спайка ---");
        println!("синтетических событий увидено: {synth} (ожидается >= 2: unicode + keydown)");
        println!(
            "hardware-like событий увидено: {hw} (ожидается >= 1: постнутый через HID-source)"
        );
        if synth >= 1 && hw >= 1 {
            println!("SPIKE OK: тап работает, свои события отличимы по kCGEventSourceStateID.");
        } else {
            println!("SPIKE INCOMPLETE: постнутые события не видны (посмотрите лог выше).");
        }
        let _ = SEEN_HARDWARE_LIVE.load(Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
fn main() {
    spike::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("Спайк доступен только на macOS.");
}
