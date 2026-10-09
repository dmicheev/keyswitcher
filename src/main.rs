//! Точка входа PuntoSwitcher.
//!
//! * На macOS: основной цикл на main-потоке (NSAutoreleasePool внутри опросов),
//!   периодическое определение активного окна и коррекция раскладки —
//!   аналог «умного переключения под активное окно» из Punto Switcher.
//! * На других ОС: запускается демонстрационный режим на эмуляторе
//!   (`src/stub.rs`), чтобы логику можно было проверить/протестировать где угодно.

mod config;
mod core;
mod stub;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use config::Config;
use core::{Action, Switcher, System};
use simplelog::{Config as LogConfig, LevelFilter, WriteLogger};

/// Период опроса активного окна (Punto Switcher тоже работает «с горячими клавишами»,
/// но базовый механизм — быстрый поллинг).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Флаг «работать дальше». Меняется обработчиком SIGINT/SIGTERM.
static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_term(_sig: i32) {
    // signal handler должен быть async-signal-safe: атомарная запись допустима.
    RUNNING.store(false, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        extern "C" {
            fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
        }
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        signal(SIGINT, on_term);
        signal(SIGTERM, on_term);
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

fn log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if cfg!(target_os = "macos") {
        std::path::PathBuf::from(home)
            .join("Library")
            .join("Logs")
            .join("puntoswicher.log")
    } else {
        std::path::PathBuf::from(home).join("puntoswicher.log")
    }
}

fn init_logging() {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::File::create(&path) {
        Ok(file) => {
            let _ = WriteLogger::init(LevelFilter::Info, LogConfig::default(), file);
        }
        Err(_) => {
            eprintln!("Не удалось создать журнал в {:?}, логирование в stderr", path);
            let _ = simplelog::SimpleLogger::init(LevelFilter::Info, LogConfig::default());
        }
    }
}

/// Общий цикл: создаёт переключатель поверх конкретной реализации `System`
/// и крутится до сигнала выхода. Компилируется на всех платформах.
fn run_loop<S: System>(system: &mut S, config: Config) {
    let mut switcher = Switcher::new(system, config);
    let mut last_config_check = Instant::now();
    let config_path = Config::default_path();

    while RUNNING.load(Ordering::SeqCst) {
        let action = switcher.step();
        if !matches!(action, Action::Nothing) {
            log::info!("{:?}", action);
        }

        // Горячая перезагрузка конфигурации раз в 5 секунд.
        if last_config_check.elapsed() > Duration::from_secs(5) {
            last_config_check = Instant::now();
            if config_path.exists() {
                let cfg = Config::load(&config_path);
                switcher.reload_config(cfg);
            }
        }

        std::thread::sleep(POLL_INTERVAL);
    }
    log::info!("Остановка по сигналу.");
}

fn main() {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    init_logging();
    install_signal_handlers();

    let config_path = Config::default_path();
    let config = Config::load(&config_path);
    log::info!(
        "Старт: auto_correct={}, default={:?}, idle={}s, исключений={}",
        config.auto_correct,
        config.default_layout,
        config.idle_pause_seconds,
        config.excluded_apps.len()
    );

    #[cfg(target_os = "macos")]
    {
        macos::MacSystem::setup_ns_application();
        let mut system = macos::MacSystem::new();
        run_loop(&mut system, config);
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "Внимание: реальная смена раскладки доступна только на macOS.\n\
             Запускается демонстрационный режим на эмуляторе системы (stub).\n\
             Соберите на macOS (`cargo build --release`) для рабочего переключателя."
        );
        let mut system = stub::StubSystem::new();
        system.push_app("DemoApp", "Window-1");
        run_loop(&mut system, config);
    }
}

fn print_help() {
    println!(
        "puntoswicher — автоматическая раскладка клавиатуры под активным окном\n\
         (аналог Punto Switcher для macOS, на Rust)\n\n\
         Использование:\n  \
         puntoswicher [--help]\n\n\
         Конфигурация: {}\n\
         Журнал:       {}\n\n\
         Требуемые разрешения macOS:\n\
         \x20 • Доступность (System Settings → Privacy & Security → Accessibility)\n\
         \x20   — нужна для заголовков окон; без неё переключение работает на уровне приложений.\n\n\
         Автозапуск: examples/com.puntoswicher.agent.plist (launchd)",
        Config::default_path().display(),
        log_path().display()
    );
}
