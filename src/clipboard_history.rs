//! История копирований в буфер обмена: память + файл на диске.
//!
//! Платформо-независимое ядро: хранит последние N записей (новые в начале),
//! отбрасывает дубликаты (повторное копирование поднимает запись наверх)
//! и синхронизируется с файлом рядом с `config.txt`
//! (`~/.config/smartswitcher/clipboard_history.txt`).
//!
//! Формат файла — по одной записи на строку; переводы строк и служебные
//! символы экранируются (`\\`, `\n`, `\r`, `\t`), поэтому многострочные
//! копирования переживают сохранение/загрузку без внешних зависимостей.

use std::collections::VecDeque;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;

/// Записи длиннее этого значения (в символах) не сохраняем: гигантские
/// копирования (дампы, base64-файлы) раздули бы и память, и файл.
const MAX_ENTRY_CHARS: usize = 100_000;

/// История буфера обмена. Новые записи — в начале очереди.
#[derive(Debug)]
pub struct ClipboardHistory {
    entries: VecDeque<String>,
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
    pub fn entries(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(String::as_str)
    }

    /// Запись по индексу (0 — самая свежая).
    pub fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    /// Добавляет копирование. Возвращает `true`, если история изменилась.
    ///
    /// Пропускаются: пустые/пробельные тексты, слишком длинные записи,
    /// повтор текущей свежей записи. Повтор старой записи поднимает её наверх.
    pub fn push(&mut self, text: &str) -> bool {
        if text.trim().is_empty() || text.chars().count() > MAX_ENTRY_CHARS {
            return false;
        }
        if self.entries.front().map(String::as_str) == Some(text) {
            return false;
        }
        self.entries.retain(|e| e != text);
        self.entries.push_front(text.to_string());
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
            ok &= write_escaped_line(&mut out, entry).is_ok();
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
            let entry = unescape_line(&line);
            // Идём от старых к новым: каждый push кладёт запись наверх.
            self.entries.push_back(entry);
        }
        // Убираем дубликаты (файл мог быть отредактирован руками), оставляя
        // самые свежие: строки идут от новых к старым, первый дубль — новейший.
        let mut deduped: VecDeque<String> = VecDeque::with_capacity(self.entries.len());
        for entry in &self.entries {
            if !deduped.contains(entry) {
                deduped.push_back(entry.clone());
            }
        }
        self.entries = deduped;
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }
}

/// Экранирует служебные символы и пишет строку записи.
fn write_escaped_line<W: Write>(out: &mut W, entry: &str) -> std::io::Result<()> {
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
    out.write_all(b"\n")
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
        assert!(!h.push(""));
        assert!(!h.push("   \n\t "));
        assert!(h.is_empty());
    }

    #[test]
    fn push_newest_first_and_dedup() {
        let mut h = ClipboardHistory::new(5);
        assert!(h.push("первый"));
        assert!(h.push("второй"));
        assert_eq!(h.get(0), Some("второй"));
        assert_eq!(h.get(1), Some("первый"));
        // Повтор свежей записи — ничего не меняется.
        assert!(!h.push("второй"));
        // Повтор старой записи поднимает её наверх.
        assert!(h.push("первый"));
        assert_eq!(h.get(0), Some("первый"));
        assert_eq!(h.get(1), Some("второй"));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn capacity_trims_oldest() {
        let mut h = ClipboardHistory::new(2);
        for (i, text) in ["a", "b", "c"].into_iter().enumerate() {
            assert!(h.push(text), "push #{i}");
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(0), Some("c"));
        assert_eq!(h.get(1), Some("b"));
    }

    #[test]
    fn set_capacity_grows_and_shrinks() {
        let mut h = ClipboardHistory::new(3);
        for text in ["a", "b", "c"] {
            h.push(text);
        }
        h.set_capacity(1);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get(0), Some("c"));
        h.set_capacity(10);
        h.push("d");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn oversized_entries_are_skipped() {
        let mut h = ClipboardHistory::new(5);
        let huge = "x".repeat(MAX_ENTRY_CHARS + 1);
        assert!(!h.push(&huge));
        assert!(h.push("ok"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn clear_empties_history() {
        let mut h = ClipboardHistory::new(5);
        h.push("a");
        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn save_load_roundtrip_with_multiline() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let mut h = ClipboardHistory::new(10);
        h.push("одна строка");
        h.push("строка 1\nстрока 2\r\nс табом\t и бэкслешем \\ в конце\n");
        h.push(" PruF: \\n это буквально бэкслеш-n ");
        h.save(&path);

        let mut loaded = ClipboardHistory::new(10);
        loaded.load(&path);
        assert_eq!(loaded.len(), h.len());
        let originals: Vec<&str> = h.entries().collect();
        let restored: Vec<&str> = loaded.entries().collect();
        assert_eq!(originals, restored);

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
        assert_eq!(h.get(0), Some("a"));
        assert_eq!(h.get(1), Some("b"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_is_noop() {
        let mut h = ClipboardHistory::new(3);
        h.load(&tmp_path("no-such-file"));
        assert!(h.is_empty());
    }
}
