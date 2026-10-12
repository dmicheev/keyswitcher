//! История копирований в буфер обмена: память + файл на диске.
//!
//! Платформо-независимое ядро: хранит последние N записей — текст и момент
//! копирования (unix-секунды; показывается в меню, не участвует во вставке),
//! отбрасывает дубликаты (повторное копирование поднимает запись наверх и
//! обновляет время) и синхронизируется с файлом рядом с `config.txt`
//! (`~/.config/smartswitcher/clipboard_history.txt`).
//!
//! Формат файла — по одной записи на строку: `<unix-время>\t<текст>`, где
//! перевод строк и служебные символы текста экранируются (`\\`, `\n`, `\r`,
//! `\t`). Строки без таба (старый формат) читаются как текст без времени.

use std::collections::VecDeque;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;

/// Записи длиннее этого значения (в символах) не сохраняем: гигантские
/// копирования (дампы, base64-файлы) раздули бы и память, и файл.
const MAX_ENTRY_CHARS: usize = 100_000;

/// Одна запись истории: текст копирования и момент (unix-секунды).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipEntry {
    pub text: String,
    /// Момент копирования (unix-время, секунды). `0` — время неизвестно
    /// (запись из файла старого формата).
    pub at: u64,
}

/// История буфера обмена. Новые записи — в начале очереди.
#[derive(Debug)]
pub struct ClipboardHistory {
    entries: VecDeque<ClipEntry>,
    capacity: usize,
}

impl ClipboardHistory {
    /// Пустая история с ёмкостью `capacity` (минимум 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Меняет ёмкость «на горячую» (перезагрузка config.txt), обрезая лишнее.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }

    /// Количество записей в истории.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` — история пуста.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Записи, от новых к старым.
    pub fn entries(&self) -> impl Iterator<Item = &ClipEntry> {
        self.entries.iter()
    }

    /// Запись по индексу (0 — самая свежая).
    pub fn get(&self, index: usize) -> Option<&ClipEntry> {
        self.entries.get(index)
    }

    /// Добавляет копирование с моментом `at` (unix-секунды). Возвращает
    /// `true`, если история изменилась.
    ///
    /// Пропускаются: пустые/пробельные тексты, слишком длинные записи,
    /// повтор свежей записи. Повтор старой записи поднимает её наверх
    /// и обновляет время.
    pub fn push_at(&mut self, text: &str, at: u64) -> bool {
        if text.trim().is_empty() || text.chars().count() > MAX_ENTRY_CHARS {
            return false;
        }
        if let Some(front) = self.entries.front() {
            if front.text == text {
                // Повтор свежей записи: обновляем только время.
                if front.at == at {
                    return false;
                }
                if let Some(front) = self.entries.front_mut() {
                    front.at = at;
                }
                return true;
            }
        }
        self.entries.retain(|e| e.text != text);
        self.entries.push_front(ClipEntry {
            text: text.to_string(),
            at,
        });
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
        true
    }

    /// Очищает историю.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Сохраняет историю в файл (каталог создаётся при необходимости).
    /// Ошибки не всплывают наверх: потеря файла истории не должна ломать агент.
    pub fn save(&self, path: &Path) {
        let dir_ok = path.parent().map(std::fs::create_dir_all).transpose();
        let file = dir_ok.and_then(|_| std::fs::File::create(path));
        let Ok(file) = file else {
            log::warn!("Не удалось сохранить историю буфера в {path:?}");
            return;
        };
        let mut out = BufWriter::new(file);
        let mut ok = true;
        for entry in &self.entries {
            ok &= write!(out, "{}\t", entry.at).is_ok();
            ok &= write_escaped_body(&mut out, &entry.text).is_ok();
            ok &= out.write_all(b"\n").is_ok();
        }
        ok &= out.flush().is_ok();
        if !ok {
            log::warn!("Не удалось записать историю буфера в {path:?}");
        }
    }

    /// Догружает историю из файла (если он есть) поверх текущей:
    /// записи из файла добавляются в конец, ёмкость соблюдается.
    pub fn load(&mut self, path: &Path) {
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            // Формат: `<unix-время>\t<экранированный текст>` (таб внутри
            // текста экранирован, разделитель однозначен). Строка без таба —
            // старый формат: весь текст, время неизвестно (0).
            let (at, text) = match line.split_once('\t') {
                Some((head, rest))
                    if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) =>
                {
                    (head.parse().unwrap_or(0), rest.to_string())
                }
                _ => (0, line.clone()),
            };
            let entry = unescape_line(&text);
            // Идём от старых к новым: каждый push кладёт запись наверх.
            self.entries.push_back(ClipEntry { text: entry, at });
        }
        // Убираем дубликаты (файл мог быть отредактирован руками), оставляя
        // самые свежие: строки идут от новых к старым, первый дубль — новейший.
        let mut deduped: VecDeque<ClipEntry> = VecDeque::with_capacity(self.entries.len());
        for entry in &self.entries {
            if !deduped.iter().any(|e| e.text == entry.text) {
                deduped.push_back(entry.clone());
            }
        }
        self.entries = deduped;
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }
}

/// Экранирует служебные символы записи (без завершающего перевода строки).
fn write_escaped_body<W: Write>(out: &mut W, entry: &str) -> std::io::Result<()> {
    for c in entry.chars() {
        match c {
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            other => {
                let mut buf = [0u8; 4];
                out.write_all(other.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    Ok(())
}

/// Разбирает строку записи (обратная операция к [`write_escaped_line`]).
fn unescape_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            // Потерянный экранируемый символ — воспроизводим как есть.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "smartswitcher-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir.join("clipboard_history.txt")
    }

    #[test]
    fn push_skips_empty_and_whitespace() {
        let mut h = ClipboardHistory::new(5);
        assert!(!h.push_at("", 1));
        assert!(!h.push_at("   \n\t ", 1));
        assert!(h.is_empty());
    }

    #[test]
    fn push_newest_first_and_dedup() {
        let mut h = ClipboardHistory::new(5);
        assert!(h.push_at("первый", 100));
        assert!(h.push_at("второй", 200));
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("второй"));
        assert_eq!(h.get(1).map(|e| e.text.as_str()), Some("первый"));
        assert_eq!(h.get(0).map(|e| e.at), Some(200));
        // Повтор свежей записи с тем же временем — ничего не меняется.
        assert!(!h.push_at("второй", 200));
        // Повтор свежей записи с новым временем обновляет время.
        assert!(h.push_at("второй", 300));
        assert_eq!(h.get(0).map(|e| e.at), Some(300));
        // Повтор старой записи поднимает её наверх и обновляет время.
        assert!(h.push_at("первый", 400));
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("первый"));
        assert_eq!(h.get(0).map(|e| e.at), Some(400));
        assert_eq!(h.get(1).map(|e| e.text.as_str()), Some("второй"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn capacity_trims_oldest() {
        let mut h = ClipboardHistory::new(2);
        for (i, (text, at)) in [("a", 1u64), ("b", 2), ("c", 3)].into_iter().enumerate() {
            assert!(h.push_at(text, at), "push #{i}");
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("c"));
        assert_eq!(h.get(1).map(|e| e.text.as_str()), Some("b"));
    }

    #[test]
    fn set_capacity_grows_and_shrinks() {
        let mut h = ClipboardHistory::new(3);
        for (text, at) in [("a", 1u64), ("b", 2), ("c", 3)] {
            h.push_at(text, at);
        }
        h.set_capacity(1);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("c"));
        h.set_capacity(10);
        h.push_at("d", 4);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn oversized_entries_are_skipped() {
        let mut h = ClipboardHistory::new(5);
        let huge = "x".repeat(MAX_ENTRY_CHARS + 1);
        assert!(!h.push_at(&huge, 1));
        assert!(h.push_at("ok", 1));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn clear_empties_history() {
        let mut h = ClipboardHistory::new(5);
        h.push_at("a", 1);
        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn save_load_roundtrip_with_multiline_and_time() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut h = ClipboardHistory::new(10);
        h.push_at("одна строка", 1_700_000_000);
        h.push_at(
            "строка 1\nстрока 2\r\nс табом\t и бэкслешем \\ в конце\n",
            1_700_000_100,
        );
        h.push_at(" PruF: \\n это буквально бэкслеш-n ", 1_700_000_200);
        h.save(&path);

        let mut loaded = ClipboardHistory::new(10);
        loaded.load(&path);
        assert_eq!(loaded.len(), h.len());
        let originals: Vec<ClipEntry> = h.entries().cloned().collect();
        let restored: Vec<ClipEntry> = loaded.entries().cloned().collect();
        assert_eq!(originals, restored);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_old_format_without_time() {
        let path = tmp_path("old-format");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "без времени\n").expect("write fixture");

        let mut h = ClipboardHistory::new(5);
        h.load(&path);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("без времени"));
        assert_eq!(h.get(0).map(|e| e.at), Some(0));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn text_starting_with_digits_is_not_confused_with_timestamp() {
        let path = tmp_path("digits");
        let _ = std::fs::remove_file(&path);
        let mut h = ClipboardHistory::new(5);
        // Время 0 -> строка файла "0\t123-456"; текст "123-456" не время.
        h.push_at("123-456", 0);
        h.save(&path);
        let mut loaded = ClipboardHistory::new(5);
        loaded.load(&path);
        assert_eq!(
            loaded.get(0).map(|e| e.text.as_str()),
            Some("123-456"),
            "текст не должен съедаться разбором времени"
        );
        assert_eq!(loaded.get(0).map(|e| e.at), Some(0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_respects_capacity_and_dedup() {
        let path = tmp_path("capacity");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "a\nb\na\nc\n").expect("write fixture");

        let mut h = ClipboardHistory::new(2);
        h.load(&path);
        // Дубликат «a» (первый в файле — самый свежий) остаётся наверху,
        // ёмкость 2: a, b.
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0).map(|e| e.text.as_str()), Some("a"));
        assert_eq!(h.get(1).map(|e| e.text.as_str()), Some("b"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_is_noop() {
        let mut h = ClipboardHistory::new(3);
        h.load(&tmp_path("no-such-file"));
        assert!(h.is_empty());
    }
}
