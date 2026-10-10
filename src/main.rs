//! Точка входа SmartSwitcher.
//!
//! * На macOS: основной цикл на main-потоке (NSAutoreleasePool внутри опросов),
//!   периодическое определение активного окна и коррекция раскладки —
//!   аналог «умного переключения под активное окно» из Smart Switcher.
//! * На других ОС: запускается демонстрационный режим на эмуляторе
//!   (`src/stub.rs`), чтобы логику можно было проверить/протестировать где угодно.

mod config;
mod engine;
#[cfg(any(test, not(target_os = "macos")))]
mod stub;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use config::Config;
use engine::{Action, Switcher, System};
use simplelog::{Config as LogConfig, LevelFilter, WriteLogger};

/// Период опроса активного окна (Smart Switcher тоже работает «с горячими клавишами»,
/// но базовый механизм — быстрый поллинг).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Период горячей перезагрузки конфигурации.
const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Флаг «работать дальше». Меняется обработчиком SIGINT/SIGTERM.
pub(crate) static RUNNING: AtomicBool = AtomicBool::new(true);

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

pub(crate) fn log_path() -> std::path::PathBuf {
    // Журнал — рядом с программой (тот же каталог, что и config.txt).
    config::base_dir().join("smartswitcher.log")
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
            eprintln!(
                "Не удалось создать журнал в {:?}, логирование в stderr",
                path
            );
            let _ = simplelog::SimpleLogger::init(LevelFilter::Info, LogConfig::default());
        }
    }
}

/// Общий цикл: крутит переключатель поверх конкретной реализации `System`
/// до сигнала выхода. Компилируется на всех платформах.
fn run_loop<S: System>(system: &mut S, config: Config) {
    let mut switcher = Switcher::new(config);
    let mut last_config_check = Instant::now();
    let config_path = Config::default_path();

    while RUNNING.load(Ordering::SeqCst) {
        let action = switcher.step(system);
        if !matches!(action, Action::Nothing) {
            log::info!("{:?}", action);
        }

        // Горячая перезагрузка конфигурации.
        if last_config_check.elapsed() > CONFIG_RELOAD_INTERVAL {
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
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if args
        .iter()
        .any(|a| a == "--list-layouts" || a == "--layouts")
    {
        #[cfg(target_os = "macos")]
        macos::MacSystem::print_input_sources();
        #[cfg(not(target_os = "macos"))]
        eprintln!("Список раскладок доступен только на macOS.");
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
        let headless = args.iter().any(|a| a == "--no-ui");
        if headless {
            // «Головой» режим: как раньше, цикл со sleep и без UI.
            macos::MacSystem::setup_ns_application();
            let mut system = macos::MacSystem::new();
            run_loop(&mut system, config);
        } else {
            let system = macos::MacSystem::new();
            macos::ui::run_ui(system, config);
        }
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
        "smartswitcher — автоматическая раскладка клавиатуры под активным окном\n\
         (Smart Switcher для macOS, на Rust)\n\n\
         Использование:\n  \
         smartswitcher [--help] [--list-layouts] [--no-ui]\n\n\
         Конфигурация: {}\n\
         Журнал:       {}\n\n\
         Требуемые разрешения macOS:\n\
         \x20 • Доступность (System Settings → Privacy & Security → Accessibility)\n\
         \x20   — нужна для заголовков окон; без неё переключение работает на уровне приложений.\n\n\
         Переключение использует только ВКЛЮЧЁННЫЕ источники ввода\n\
         \x20 (System Settings → Keyboard → Input Sources); список: --list-layouts\n\n\
         Автозапуск: examples/com.smartswitcher.agent.plist (launchd)",
        Config::default_path().display(),
        log_path().display()
    );
}
