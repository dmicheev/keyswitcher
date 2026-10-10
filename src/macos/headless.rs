//! Headless-цикл (`--no-ui`) на CFRunLoop вместо sleep-поллинга.
//!
//! Основной поток крутит `CFRunLoopRun`; `CFRunLoopTimer` раз в 200 мс
//! делает шаг [`Switcher::step`] (то же поведение, что раньше), раз в 5 с
//! перезагружает конфиг и тикает конвейером режима печати (event tap живёт
//! на этом же run loop). Сигналы SIGINT/SIGTERM останавливают цикл.

use std::ffi::c_void;
use std::time::Instant;

use crate::config::Config;
use crate::engine::{Action, Switcher};
use crate::macos::event_tap::TypingPipeline;
use crate::macos::ffi;
use crate::macos::MacSystem;
use crate::{CONFIG_RELOAD_INTERVAL, POLL_INTERVAL, RUNNING};

/// Всё состояние headless-агента; доступно только из main-потока.
struct HeadlessCtx {
    system: MacSystem,
    switcher: Switcher,
    pipeline: Option<Box<TypingPipeline>>,
    config_path: std::path::PathBuf,
    last_config_check: Instant,
}

extern "C" fn on_timer(_timer: ffi::CFRunLoopTimerRef, info: *mut c_void) {
    // Safety: info — указатель на HeadlessCtx, живёт до конца run_headless.
    let ctx = unsafe { &mut *(info as *mut HeadlessCtx) };

    let action = ctx.switcher.step(&mut ctx.system);
    if !matches!(action, Action::Nothing) {
        log::info!("{:?}", action);
    }

    if ctx.last_config_check.elapsed() > CONFIG_RELOAD_INTERVAL {
        ctx.last_config_check = Instant::now();
        if ctx.config_path.exists() {
            let cfg = Config::load(&ctx.config_path);
            ctx.switcher.reload_config(cfg);
        }
    }

    if let Some(pipeline) = ctx.pipeline.as_mut() {
        pipeline.tick(&mut ctx.system, ctx.switcher.config());
    }

    if !RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
        // Safety: остановка main run loop из main-потока.
        unsafe { ffi::CFRunLoopStop(ffi::CFRunLoopGetMain()) };
    }
}

/// Запускает headless-агент (блокируется до SIGINT/SIGTERM).
pub fn run_headless(system: MacSystem, config: Config) {
    MacSystem::setup_ns_application();
    let pipeline = TypingPipeline::install(&config);

    let ctx = Box::new(HeadlessCtx {
        system,
        switcher: Switcher::new(config),
        pipeline,
        config_path: Config::default_path(),
        last_config_check: Instant::now(),
    });
    let ctx_ptr = Box::into_raw(ctx) as *mut c_void;

    unsafe {
        let context = ffi::CFRunLoopTimerContext {
            version: 0,
            info: ctx_ptr,
            retain: None,
            copyDescription: None,
            release: None,
        };
        let interval = POLL_INTERVAL.as_secs_f64();
        let timer = ffi::CFRunLoopTimerCreate(
            std::ptr::null_mut(),
            ffi::CFAbsoluteTimeGetCurrent() + interval,
            interval,
            0,
            0,
            on_timer,
            &context,
        );
        ffi::CFRunLoopAddTimer(ffi::CFRunLoopGetMain(), timer, ffi::kCFRunLoopCommonModes);
        ffi::CFRunLoopRun();
        ffi::CFRunLoopTimerInvalidate(timer);
        ffi::CFRelease(timer as ffi::CFTypeRef);
        drop(Box::from_raw(ctx_ptr as *mut HeadlessCtx));
    }
    log::info!("Остановка по сигналу.");
}
