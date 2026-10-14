//! Точка входа SmartSwitcher.
//!
//! * На macOS: основной цикл на main-потоке (NSAutoreleasePool внутри опросов),
//!   периодическое определение активного окна и коррекция раскладки —
//!   аналог «умного переключения под активное окно» из Smart Switcher.
//! * На других ОС: запускается демонстрационный режим на эмуляторе
//!   (`src/stub.rs`), чтобы логику можно было проверить/протестировать где угодно.

mod clipboard_history;
mod config;
mod dict;
mod engine;
mod keymap;
mod layout_convert;
mod ngram;
mod predictor;
mod short_words;
mod spell;
#[cfg(any(test, not(target_os = "macos")))]
mod stub;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[cfg(not(target_os = "macos"))]
use std::time::Instant;

use config::Config;
use simplelog::{Config as LogConfig, LevelFilter, WriteLogger};

#[cfg(not(target_os = "macos"))]
use engine::{Action, Switcher, System};

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
    // libc уже в дереве зависимостей (objc → malloc_buf); своя декларация
    // `signal` с неточным типом возврата была бы хрупкой.
    unsafe {
        let handler = on_term as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

pub(crate) fn log_path() -> std::path::PathBuf {
    // Журнал — в одном каталоге с конфигом (~/.config/smartswitcher).
    config::config_dir().join("smartswitcher.log")
}

fn init_logging(enabled: bool) {
    // Журнал по умолчанию выключен: вызовы log:: просто отбрасываются.
    if !enabled {
        return;
    }
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
/// до сигнала выхода. Используется на не-macOS (демо на заглушке);
/// на macOS headless-режим работает на CFRunLoop (см. `macos/headless.rs`),
/// чтобы на main run loop жил event tap режима печати.
#[cfg(not(target_os = "macos"))]
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

    // Конфиг нужен до инициализации журнала: писать его или нет — решает
    // ключ write_log. Сообщение о миграции логируем после инициализации.
    install_signal_handlers();
    let migrated = Config::migrate_portable_config();

    let config_path = Config::default_path();
    let config = Config::load(&config_path);
    init_logging(config.write_log);

    if let Some(old) = migrated {
        log::info!(
            "Конфигурация перенесена: {old:?} -> {}",
            Config::default_path().display()
        );
    }
    // Портативный режим: если конфига ещё нет — создаём с текущими
    // значениями по умолчанию, чтобы его можно было править руками.
    if !config_path.exists() {
        match config.save(&config_path) {
            Ok(()) => log::info!("Создан конфиг по умолчанию: {}", config_path.display()),
            Err(e) => log::warn!("Не удалось создать конфиг {:?}: {}", config_path, e),
        }
    }
    log::info!(
        "Старт: auto_correct={}, default={:?}, idle={}s, исключений={}, журнал={}, история буфера={}",
        config.auto_correct,
        config.default_layout,
        config.idle_pause_seconds,
        config.excluded_apps.len(),
        config.write_log,
        config.clipboard_history
    );

    #[cfg(target_os = "macos")]
    {
        // Прогрев словарей в фоне: к моменту установки тапа (после старта UI)
        // они обычно уже загружены — меню не «замирает» на время разбора.
        std::thread::spawn(|| {
            predictor::LangPair::load_embedded();
        });

        let headless = args.iter().any(|a| a == "--no-ui");
        if headless {
            // «Головой» режим: цикл на CFRunLoop (поллинг окна + event tap
            // режима печати живут на main run loop), без UI.
            macos::MacSystem::setup_ns_application();
            let system = macos::MacSystem::new();
            macos::headless::run_headless(system, config);
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
        "smartswitcher — автоматическая раскладка клавиатуры под активное окно\n\
          (Smart Switcher для macOS, на Rust)\n\n\
          Использование:\n  \
          smartswitcher [--help] [--list-layouts] [--no-ui]\n\n\
          Конфигурация: {}\n\
          Журнал:       {}\n\
          \x20 (не пишется по умолчанию; включите write_log = true в конфиге)\n\n\
          Требуемые разрешения macOS:\n\
          \x20 • Доступность (System Settings → Privacy & Security → Accessibility)\n\
          \x20   — нужна для заголовков окон; без неё переключение работает на уровне приложений.\n\
          \x20   — она же нужна режиму печати (CGEventTap); без неё функция отключается.\n\n\
          Режим печати (typing_detect в config.txt): автопереключение EN<->RU по ходу\n\
          набора + хоткеи:\n\
          \x20 • convert_hotkey (тап Option) — конвертация последнего слова или\n\
          \x20   всего выделенного текста;\n\
          \x20 • suggest_hotkey (тап Command) — варианты замены выделенного слова\n\
          \x20   в попапе, выбор цифрами 1-9 или кликом.\n\n\
          Переключение использует только ВКЛЮЧЁННЫЕ источники ввода\n\
          \x20 (System Settings → Keyboard → Input Sources); список: --list-layouts\n\n\
          Автозапуск: examples/com.smartswitcher.agent.plist (launchd)",
        Config::default_path().display(),
        log_path().display()
    );
}
