//! Короткие системные звуки событий режима печати.
//!
//! Системные NSSound по имени: «Tink» — автопереключение раскладки,
//! «Pop» — попап подсказок исправления. Воспроизведение асинхронное
//! (звуковой сервер), колбэк event tap не блокирует. Экземпляры загружаются
//! один раз и удерживаются до выхода: NSSound должен жить, пока звучит
//! (autorelease при дренировании пула остановил бы воспроизведение).

// Макросы objc 0.2 используют cfg(feature = "cargo-clippy") — глушим точечно
// (так же, как в ui.rs и system.rs).
#![allow(unexpected_cfgs)]

use std::ffi::CString;
use std::sync::OnceLock;

use objc::runtime::Object;
use objc::{class, msg_send, sel, sel_impl};

use crate::macos::system::AutoReleasePool;

type Id = *mut Object;

const NIL: Id = std::ptr::null_mut();

/// Адреса удерживаемых NSSound: (переключение, подсказки); 0 — звук
/// недоступен в системе.
static SOUNDS: OnceLock<(usize, usize)> = OnceLock::new();

/// Короткий сигнал автопереключения раскладки.
pub fn play_switch() {
    play(SOUNDS.get_or_init(load_sounds).0);
}

/// Короткий сигнал показа попапа с вариантами исправления.
pub fn play_suggest() {
    play(SOUNDS.get_or_init(load_sounds).1);
}

fn play(sound: usize) {
    if sound == 0 {
        return;
    }
    // Safety: вызовы только с main-потока (колбэки тапа и run loop);
    // объект удержан в статике, play — асинхронный.
    unsafe {
        let _: bool = msg_send![sound as Id, play];
    }
}

/// Загружает оба звука; при недоступности имени — 0 (тишина вместо ошибки).
fn load_sounds() -> (usize, usize) {
    // Пул на случай первого вызова вне autorelease-контекста (колбэк тапа):
    // NSString/NSSound создают autorelease-объекты, звуки удерживаем retain'ом.
    let _pool = AutoReleasePool::new();
    // Safety: main-поток, AppKit.
    unsafe { (retain_sound("Tink"), retain_sound("Pop")) }
}

/// Safety: main-поток, живой AppKit-рантайм.
unsafe fn retain_sound(name: &str) -> usize {
    let c = CString::new(name).expect("NUL в имени звука");
    let name: Id = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
    let sound: Id = msg_send![class!(NSSound), soundNamed: name];
    if sound == NIL {
        return 0;
    }
    let _: Id = msg_send![sound, retain];
    sound as usize
}
