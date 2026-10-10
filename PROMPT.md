# ПРОМТ: Автопереключение раскладки по ходу печати (word prediction)

Ты — агент-разработчик. Реализуй в этом репозитории новую функциональность по спецификации ниже.
Проект рабочий, не ломай существующее поведение и тесты. Читай AGENTS.md: перед работой с Rust-кодом
загружай скилл `rust-best-practices`; перед завершением обязательны quality gates:

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test          # (эквивалент: make check)
```

---

## 1. Контекст проекта

`smartswitcher` (Rust, edition 2021, только std + log/simplelog + objc 0.2 на macOS) — переключатель
раскладки EN/RU для macOS. Текущая архитектура:

- `src/engine.rs` — платформо-независимое ядро: `Layout {En,Ru,Other}`, трейт `System`
  (`active_window`, `current_layout`, `set_layout`), `Switcher` — память раскладок по окнам,
  поллинг активного окна раз в 200 мс. Юнит-тесты на заглушке `StubSystem`.
- `src/config.rs` — `ключ = значение` конфиг `config.txt` рядом с бинарником (портативный режим),
  горячая перезагрузка раз в 5 с.
- `src/macos/ffi.rs` — обёртки Carbon TIS (`TISCopyCurrentKeyboardInputSource`,
  `TISCreateInputSourceList`, `TISSelectInputSource`, `kTISPropertyInputSourceID`, ...).
- `src/macos/system.rs` — `MacSystem: System` (NSWorkspace, TIS, заголовок окна через AX-мост).
  Раскладки классифицируются по ID (`...keylayout.Russian` → Ru, `...ABC/US` → En).
- `src/macos/ui.rs` — иконка меню-бара, меню, диалог настроек.
- `src/main.rs` — цикл (headless-режим `--no-ui` со `sleep`, иначе `run_ui` с NSApplication).
- `build.rs` — компилирует `assets/ax_bridge.c` (cc), линкует фреймворки CoreFoundation, Carbon,
  AppKit, ApplicationServices.
- `Makefile` — `make build/run/test/check/bundle/...`.

Стиль кода: подробные doc-комментарии `///` на русском в шапках модулей, `// Safety:` перед unsafe,
unit-тесты в `#[cfg(test)] mod tests` рядом с кодом. Соблюдай его.

**Сейчас переключения во время печати НЕТ** — есть только раскладка по окнам. Реализуем её.

## 2. Цель (решение пользователя, зафиксировано)

Во время печати по потоку физических клавиш определять, на каком языке вероятнее всего пользователь
собирается набрать слово, и:

1. **Автопереключение с ретро-фиксом** (основной режим, «Punto-стиль»): при уверенном определении
   переключить системную раскладку на правильный язык И исправить уже набранный «неправильный»
   префикс слова (backspace × k + ввод правильных символов).
2. **Хоткей-конвертация последнего слова**: глобальная горячая клавиша конвертирует последнее слово
   в другую раскладку (работает и при выключенном автопереключении).

Зафиксированные решения:
- Словари: **FrequencyWords (OpenSubtitles)**, top **~200k** слов на язык (ru, en).
- **N-граммная fallback-модель сразу** (для слов вне словаря: имена, транслит, опечатки).
- Архитектурно — **любые пары языков** (данные отделены от логики), v1 поставляется с EN/RU.
- `retro_fix` отключаемый (fallback = просто переключить раскладку, префикс не чиним).

## 3. Технический дизайн

### 3.1 Захват клавиш

- **Активный event tap**: `CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap,
  kCGEventTapOptionDefault, mask = keyDown | flagsChanged, callback, ctx)` — активный тап может
  изменять/поглощать события. Обёртки — чистые `extern "C"` декларации в новом `src/macos/ffi.rs`
  (или отдельном модуле), в `build.rs` добавить `println!("cargo:rustc-link-lib=framework=CoreGraphics")`.
- Тап вешается на **main runloop** (в UI-режиме он уже есть у NSApplication; в headless-режиме
  `--no-ui` заменить sleep-цикл на `CFRunLoopRun` + `CFRunLoopTimer` для поллинга/конфига —
  сохранив текущее поведение `Switcher::step`).
- **Callback** (C ABI): быстрый, без блокировок. Возвращает событие (возможно изменённое) или `NULL`
  для поглощения.
- Обрабатывать только `keyDown` без модификаторов кроме Shift (Cmd/Ctrl/Opt/NumLock → проходят
  нетронутыми — не ломать хоткеи). `flagsChanged` — только для отслеживания состояния модификаторов.
- **Свои инжектированные события пропускать** (защита от цикла). Варианты — проверить оба в спайке:
  (а) `CGEventGetIntegerValueField(ev, kCGEventSourceStateID)`: hardware-события имеют состояние
  HID-системы, события, постнутые через source `kCGEventSourceStateCombinedSessionState`, — другое;
  (б) magic-значение в «пользовательском» поле события при постинге. Постить события через
  `CGEventSourceCreate(kCGEventSourceStateCombinedSessionState)` + `CGEventPost(kCGSessionEventTap, ...)`.
- **Secure Input** (поля паролей): тап перестаёт получать события / диспаблится. Обработать
  `kCGEventTapDisabledByTimeout`/`kCGEventTapDisabledByUserInput` → ре-включение тапа; молчаливая
  деградация (ничего не переключать, не падать).
- Разрешения: нужен Accessibility (уже запрашивается). Если тап создать не удалось — лог warning,
  функция отключена, остальное приложение работает.
- Не вмешиваться, когда наше приложение активно (свой диалог настроек) и для приложений из
  `exclude_apps` (переиспользовать `Config::is_excluded` и активное окно).

### 3.2 Keymap: keycode → символ в каждой раскладке

- Основной путь — runtime из системы: для включённых EN/RU источников ввода взять
  `TISGetInputSourceProperty(src, kTISPropertyUnicodeKeyLayoutData)` → `CFData` с `UCKeyboardLayout`,
  и `UCKeyTranslate(layout, keycode, kUCKeyActionDown, modifiers | kUCKeyTranslateNoDeadKeysBit,
  ...)` → символ (учесть Shift; capsLock). Нужно добавить декларации в ffi (Carbon HIToolbox:
  `UCKeyTranslate`, типы `UCKeyboardLayout`, `UniCharCount`...).
- Fallback (и для unit-тестов без macOS): статические таблицы US QWERTY и ЙЦУКЕН
  (буквы, сдвиг-варианты). Код клавиш — HID Usage (например, Q=0x0C=12, П/'=0x13=19...).
- Интерфейс уровня ядра: `Keymap { fn chars_for(keycode: u8, shift: bool) -> Option<(Option<char> en, Option<char> ru)> }`
  (в терминах v1; при обобщении — map язык→Option<char>).

### 3.3 Словари и данные

- Python-подпроект `tools/dict/prepare.py` (зависимости: только stdlib/requests):
  скачивает списки FrequencyWords (hermitdave/FrequencyWords; файлы вида
  `content/2018/ru/ru_50k.txt` и `content/2016/ru/ru_full.txt` — проверь фактическую доступность
  и возьми максимальные доступные полные списки), затем:
  lowercase; оставить только буквенные токены (en: `[a-z']`, ru: `[а-яё-]`, ё→е НЕ нормализуем —
  храним как есть); дедуп с суммированием частот; отсортировать; отрезать top-200000;
  записать `assets/lang/ru.tsv`, `assets/lang/en.tsv` (`word\tcount`).
  Сделай цель `make dicts` и опиши в README.
- Встраивание: `include_bytes!("../assets/lang/ru.tsv")` и т.д. (портативность важна,
  рост бинарника на ~4–8 МБ приемлем). Парсинг при старте (~2×200k строк, <100 мс — ок).

### 3.4 Структуры данных и скоринг (ядро, без macOS)

Новые модули в `src/` (платформо-независимые, полностью unit-тестируемые):

- `dict.rs` — `FrequencyDict::load(&str)`, точный поиск `freq(word)` и **префиксный** запрос
  `max_freq_prefix(prefix)`. Реализация на выбор: sorted `Vec<(String,u64)>` + sparse-table RMQ по
  массиву частот (компактно, O(1) запрос) или arena-trie с max-subtree-freq. Линейный скан диапазона
  недопустим (короткие префиксы покрывают десятки тысяч слов).
- `ngram.rs` — char-3-граммная модель с маркерами границ `^`/`$`, строится **при загрузке** из того же
  словаря с весами частот (Laplace-сглаживание, всё в log-домене). API: `score(word: &str) -> f64`.
- `predictor.rs` — стейт-машина слова:
  - input: `keycode`, `shift`, `current_layout`, `layout_changed_by_user` (сброс доверия при ручном
    переключении — иначе будем «побороть» пользователя);
  - буфер: пары (en_char, ru_char) текущего слова; сброс по разделителю (space/enter/tab/пунктуация),
    по цифре/символу внутри слова (кроме дефиса/апострофа) и по паузе >N сек;
  - скоринг после каждого символа начиная с `min_prefix` (по умолчанию 3):
    `score(lang) = log(max_freq_prefix(w_lang))` при ненулевом покрытии словарём, иначе
    `α·ngram_score(w_lang)`; при полном совпадении слова — бонус. Оба слова в словаре → сравнение
    частот; одно в словаре, другое нет → словарное побеждает (n-граммы только при уверенном отрыве);
    ни одного → решают n-граммы;
  - решение: если `best != current_layout` и `score(best) − score(current) ≥ log(threshold)`
    (threshold по умолчанию 100) → `Decision::SwitchAndFix { layout, replacement_prefix: String }`.
    **Гистерезис: максимум одно автопереключение на слово**, после него до разделителя не решаем;
  - API для хоткея: `convert_last_word(current_layout) -> Option<String>` (последнее зафиксированное
    слово, перекодированное в другую раскладку);
  - приватность: слова из буфера НЕ писать в лог уровнем info; только debug/trace.

### 3.5 Переключение и ретро-фикс (macOS)

При `Decision::SwitchAndFix`:
1. `System::set_layout(target)` (уже есть);
2. если `retro_fix = true`: постить `backspace` × k (HID keycode 0x33=51, пары keyDown/keyUp) и
   затем ОДНО unicode-событие с исправленным префиксом
   (`CGEventCreateKeyboardEvent(source, 0, true)` + `CGEventKeyboardSetUnicodeString(ev, len, utf16)`
   + keyUp) — все через tagged source (см. 3.1), чтобы тап их не анализировал.
3. если `retro_fix = false` — только переключение (стратегия A).

Хоткей: `RegisterEventHotKey` (Carbon) + `InstallEventHandler` на main-потоке; дефолт `F13`,
настраивается строкой вида `convert_hotkey = f13` (поддержать минимум f1–f19 и модификаторы
`ctrl+shift+f13`). Обработчик: взять `convert_last_word` у предиктора → backspace × len + ввод
конвертированного слова (unicode-событием). Работает независимо от `typing_detect`.

### 3.6 Конфигурация

Новые ключи `config.txt` (+ парсер + дефолты + README):

```text
typing_detect      = false   # автопереключение по печати (включи после обкатки)
min_prefix_chars   = 3
switch_threshold   = 100.0
retro_fix          = true
convert_hotkey     = f13
```

`typing_detect = false` по умолчанию: фича перехватывает клавиатуру, включай осознанно после
ручной проверки. `exclude_apps` применяется и к ней. В диалог настроек (`ui.rs`) добавь минимум:
чекбокс `typing_detect` и `retro_fix`; остальное — через config.txt.

## 4. Порядок реализации

0. **Спайк-проверки (сделай первым, до основной массы кода)**: активный тап видит keyDown с
   keycode; постнутые тобой unicode-события реально вводят текст в TextEdit/браузер; способ
   различения своих событий (3.1) работает. Если что-то из этого не работает — остановись и
   доложи, не пиши остальное.
1. `tools/dict` + TSV-словари в `assets/lang/` + `make dicts`.
2. Ядро: `dict.rs`, `ngram.rs`, `keymap.rs` (статические таблицы), `predictor.rs` + тесты
   (см. §5). Тесты не должны требовать macOS и сети.
3. macOS: ffi для CGEventTap/UCKeyTranslate/RegisterEventHotKey, `macos/event_tap.rs`,
   runtime keymap, интеграция в main/ui (runloop-рефакторинг headless-режима), ретро-фикс, хоткей.
4. Конфиг + UI + README (раздел про режим печати, разрешения, хоткей).
5. Прогнать quality gates, почистить, обновить структуру проекта в README.

## 5. Тесты (обязательные сценарии ядра)

- Поток «ghbdtn» при layout=En → на 3-м символе `SwitchAndFix { Ru, "при" }`, повторных решений
  до разделителя нет.
- «привет» при layout=Ru (русская раскладка, русское слово) → никаких решений.
- Коллизия, валидная в обоих словарях («как»/«rfr», «bag»/«ифп» и т.п.): при достаточном отрыве
  частот переключает, при близких частотах — не трогает (порог).
- Слово вне словарей, но с явным n-граммным перекосом («dftnktq» → Ru) → переключает.
- Цифра/символ внутри слова, Cmd/Ctrl-модификаторы, ручная смена раскладки → сброс/игнор.
- `convert_last_word`: «ghbdtn» → «привет» и обратно «здравствуйте» → «plhbdn...» (обратная
  перекодировка по keymap).
- Парсер конфига: новые ключи, дефолты, hot_reload.

## 6. Риски и деградация

- Автокомплит полей (Google Docs, IDE) может конфликтовать с ретро-фиксом → `retro_fix=false`.
- Тап выключается системой по таймауту → re-enable в callback + watchdog.
- Нет разрешений/Secure Input → функция выключена, приложение живое, лог разъясняет.
- Постед-события не принимаются отдельными приложениями (игры, RDP) → fallback «только
  переключение», задокументировать.

## 7. Критерии приёмки

- `make check` зелёный; новые модули покрыты тестами по §5.
- Ручная проверка на macOS: TextEdit/Notes/браузер — «ghbdtn» превращается в «привет» без следа
  латиницы; Cmd+C/Cmd+V не затрагиваются; поле пароля (Secure Input) не затрагивается; Terminal
  из `exclude_apps` не затрагивается; хоткей конвертирует последнее слово; повторный запуск и
  остановка по сигналам не сломаны.
