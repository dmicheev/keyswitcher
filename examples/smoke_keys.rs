//! Вспомогательный инструмент ручной проверки режима печати.
//!
//! 1. Печатает ID текущего источника ввода (раскладки).
//! 2. Постит «аппаратноподобные» нажатия (source = HID system state):
//!    `ghb` — как будто пользователь начал набирать «ghbdtn» в EN-раскладке.
//!
//! Использование: запустите агент (`cargo run` или `--no-ui`) с
//! `typing_detect = true`, затем `cargo run --example smoke_keys` и понаблюдайте
//! за журналом агента: должно появиться переключение EN -> RU и ретро-фикс.
//! Ключи: `--keys ghb` (символы EN-вида), `--layout-only` (только раскладка).

#[cfg(target_os = "macos")]
mod smoke {
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::thread::sleep;
    use std::time::Duration;

    use core::ffi::c_int;

    type CGEventRef = *mut c_void;
    type CGEventSourceRef = *mut c_void;

    extern "C" {
        fn CGEventSourceCreate(stateID: c_int) -> CGEventSourceRef;
        fn CGEventCreateKeyboardEvent(
            source: CGEventSourceRef,
            virtualKey: u16,
            keyDown: bool,
        ) -> CGEventRef;
        fn CGEventSetType(event: CGEventRef, eventType: u32);
        fn CGEventSetFlags(event: CGEventRef, flags: u64);
        fn CGEventPost(tapLocation: c_int, event: CGEventRef);
        fn CFRelease(cf: *const c_void);
        fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *mut c_void;
        static kTISPropertyInputSourceID: *const c_void;
    }

    /// macOS-коды букв (QWERTY), enough for smoke-клавиш; `f13` — отдельная клавиша.
    fn keycode_of(c: char) -> Option<u16> {
        Some(match c.to_ascii_lowercase() {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            'u' => 0x20,
            'i' => 0x22,
            'o' => 0x1F,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            'k' => 0x28,
            'n' => 0x2D,
            'm' => 0x2E,
            _ => return None,
        })
    }

    /// Разбор строки: слова из букв (последовательности клавиш) и
    /// специальные токены `f13`, разделённые запятой: `the,f13`.
    fn parse_keys(s: &str) -> Vec<u16> {
        const F_KEYS: [u16; 19] = [
            0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B,
            0x71, 0x6A, 0x40, 0x4F, 0x50,
        ];
        let mut out = Vec::new();
        for token in s.split(',').filter(|t| !t.is_empty()) {
            if let Some(rest) = token.strip_prefix('f') {
                if let Ok(n) = rest.parse::<u32>() {
                    if (1..=19).contains(&n) {
                        out.push(F_KEYS[(n - 1) as usize]);
                        continue;
                    }
                }
            }
            for c in token.chars() {
                match keycode_of(c) {
                    Some(code) => out.push(code),
                    None => println!("нет кода для '{c}', пропуск"),
                }
            }
        }
        out
    }

    pub fn run(args: &[String]) {
        unsafe {
            let src = TISCopyCurrentKeyboardInputSource();
            if !src.is_null() {
                let prop = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
                // Быстро печатаем CFStringRef как C-строку в UTF-8.
                extern "C" {
                    fn CFStringGetCString(
                        s: *const c_void,
                        buf: *mut i8,
                        len: isize,
                        encoding: u32,
                    ) -> u8;
                }
                let mut buf = [0i8; 256];
                if CFStringGetCString(prop, buf.as_mut_ptr(), 256, 0x0800_0100) == 1 {
                    let id = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy();
                    println!("текущая раскладка: {id}");
                }
                CFRelease(src);
            }
            if args.iter().any(|a| a == "--layout-only") {
                return;
            }
            let keys = args
                .iter()
                .position(|a| a == "--keys")
                .and_then(|p| args.get(p + 1))
                .map(String::as_str)
                .unwrap_or("ghb");
            let hid = CGEventSourceCreate(1); // kCGEventSourceStateHIDSystemState
            if hid.is_null() {
                println!("не удалось создать HID source");
                return;
            }
            let codes = parse_keys(keys);
            println!("постим «{keys}» как аппаратные нажатия...");
            for code in codes {
                for &down in &[true, false] {
                    let ev = CGEventCreateKeyboardEvent(hid, code, down);
                    CGEventPost(1, ev); // kCGSessionEventTap
                    CFRelease(ev);
                }
                sleep(Duration::from_millis(80));
            }
            CFRelease(hid);

            // Тап модификатора: flagsChanged option-down (0x3A, flags=0x80000)
            // и через 250 мс option-up (flags=0) — как чистое нажатие-отпускание.
            if args.iter().any(|a| a == "--tap-option") {
                let hid2 = CGEventSourceCreate(1);
                if hid2.is_null() {
                    println!("не удалось создать HID source");
                    return;
                }
                println!("постим тап Option (flagsChanged down/up)...");
                const KVK_OPTION: u16 = 0x3A;
                const FLAGS_CHANGED: u32 = 12;
                const OPTION_MASK: u64 = 0x80000;
                let down = CGEventCreateKeyboardEvent(hid2, KVK_OPTION, true);
                CGEventSetType(down, FLAGS_CHANGED);
                CGEventSetFlags(down, OPTION_MASK);
                CGEventPost(1, down);
                CFRelease(down);
                sleep(Duration::from_millis(250));
                let up = CGEventCreateKeyboardEvent(hid2, KVK_OPTION, false);
                CGEventSetType(up, FLAGS_CHANGED);
                CGEventSetFlags(up, 0);
                CGEventPost(1, up);
                CFRelease(up);
                CFRelease(hid2);
            }
            println!("готово");
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    smoke::run(&args);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("smoke_keys доступен только на macOS.");
}
