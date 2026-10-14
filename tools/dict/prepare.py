#!/usr/bin/env python3
"""Подготовка частотных словарей для режима автопереключения раскладки.

Скачивает списки FrequencyWords (OpenSubtitles, https://github.com/hermitdave/FrequencyWords),
чистит, дедуплицирует и пишет `assets/lang/<lang>.tsv` (`word\\tcount`), откуда их
забирает `include_bytes!` в бинарнике smartswitcher.

Обработка токена:
  * lowercase;
  * en: только `[a-z']+`, ru: только `[а-яё-]+` (ё НЕ нормализуем — храним как есть);
  * дедуп с суммированием частот;
  * сортировка по частоте, отрезаем top-200000.

Второй слой — орфографические словари `assets/lang/<lang>_ortho.bin`: полные
списки форм из Hunspell-словарей (LibreOffice, https://github.com/LibreOffice/dictionaries),
раскрытые утилитой `unmunch`. Слой отвечает на вопрос «это вообще слово?»
(спеллчек, предиктор), частоты из субтитров — «какое слово вероятнее»
(ранжирование). Слова пишутся отсортированными, с префиксным сжатием
(front-coding), формат см. в `src/ortho.rs`.

Зависимости: stdlib + `unmunch` (macOS: `brew install hunspell`,
Debian/Ubuntu: `apt install hunspell-tools`). Запуск: `make dicts` или
`python3 tools/dict/prepare.py`. Скачанные сырые файлы кэшируются в
`tools/dict/cache/`.
"""

from __future__ import annotations

import re
import shutil
import struct
import subprocess
import sys
import urllib.request
from collections import defaultdict
from pathlib import Path

TOP = 200_000
LANGS = ("en", "ru")
# Кандидаты URL в порядке приоритета: сначала самые полные свежие списки.
BASE = "https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content"
URL_CANDIDATES = [
    "{base}/2018/{lang}/{lang}_full.txt",
    "{base}/2016/{lang}/{lang}_full.txt",
    "{base}/2018/{lang}/{lang}_50k.txt",
    "{base}/2016/{lang}/{lang}_50k.txt",
]
# Hunspell-словари (каталог в репозитории LibreOffice + базовое имя файлов).
ORTHO_SOURCES = {
    "en": "en/en_US",
    "ru": "ru_RU/ru_RU",
}
ORTHO_BASE = "https://raw.githubusercontent.com/LibreOffice/dictionaries/master"
# Шаг разреженного индекса бинарного орфо-словаря (слов на точку индекса).
ORTHO_STRIDE = 32
TOKEN_RE = {
    "en": re.compile(r"^[a-z']+$"),
    "ru": re.compile(r"^[а-яё-]+$"),
}

ROOT = Path(__file__).resolve().parents[2]
CACHE = Path(__file__).resolve().parent / "cache"
OUT_DIR = ROOT / "assets" / "lang"


def download(lang: str) -> Path:
    """Качает первый доступный список для языка, использует кэш."""
    CACHE.mkdir(parents=True, exist_ok=True)
    for tpl in URL_CANDIDATES:
        url = tpl.format(base=BASE, lang=lang)
        cache_path = CACHE / f"{lang}_{url.rsplit('/', 1)[-1]}"
        if cache_path.exists() and cache_path.stat().st_size > 0:
            print(f"  cache: {cache_path}")
            return cache_path
        try:
            print(f"  скачиваю {url} ...")
            with urllib.request.urlopen(url, timeout=60) as resp:
                data = resp.read()
        except OSError as e:
            print(f"    недоступен ({e})")
            continue
        cache_path.write_bytes(data)
        return cache_path
    raise SystemExit(f"не удалось скачать список для '{lang}'")


def prepare(lang: str) -> int:
    print(f"[{lang}]")
    src = download(lang)
    freq: defaultdict[str, int] = defaultdict(int)
    total = 0
    with src.open(encoding="utf-8") as fh:
        for line in fh:
            parts = line.split()
            if len(parts) != 2:
                continue
            word, count = parts[0].lower(), parts[1]
            if not TOKEN_RE[lang].match(word):
                continue
            if not count.isdigit():
                continue
            total += 1
            freq[word] += int(count)
    print(f"  токенов: {total}, уникальных: {len(freq)}")
    top = sorted(freq.items(), key=lambda kv: (-kv[1], kv[0]))[:TOP]
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out = OUT_DIR / f"{lang}.tsv"
    with out.open("w", encoding="utf-8") as fh:
        for word, count in top:
            fh.write(f"{word}\t{count}\n")
    print(f"  записан {out.relative_to(ROOT)} ({len(top)} слов)")
    return len(top)


def download_hunspell(lang: str) -> tuple[Path, Path]:
    """Качает пару `.aff`/`.dic` Hunspell-словаря для языка, использует кэш."""
    CACHE.mkdir(parents=True, exist_ok=True)
    source = ORTHO_SOURCES[lang]
    stem = source.rsplit("/", 1)[-1]
    paths = []
    for ext in (".aff", ".dic"):
        url = f"{ORTHO_BASE}/{source}{ext}"
        cache_path = CACHE / f"{lang}_{stem}{ext}"
        if not (cache_path.exists() and cache_path.stat().st_size > 0):
            print(f"  скачиваю {url} ...")
            with urllib.request.urlopen(url, timeout=60) as resp:
                data = resp.read()
            cache_path.write_bytes(data)
        else:
            print(f"  cache: {cache_path}")
        paths.append(cache_path)
    return paths[0], paths[1]


def expand_hunspell(lang: str, aff: Path, dic: Path) -> list[str]:
    """Раскрывает леммы с аффиксами в полный список форм утилитой `unmunch`."""
    unmunch = shutil.which("unmunch")
    if unmunch is None:
        raise SystemExit(
            "не найден `unmunch` (раскрытие аффиксов Hunspell): "
            "macOS: brew install hunspell; Debian/Ubuntu: apt install hunspell-tools"
        )
    proc = subprocess.run(
        [unmunch, str(dic), str(aff)],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    forms = set()
    for word in proc.stdout.splitlines():
        word = word.strip().lower()
        if TOKEN_RE[lang].match(word):
            forms.add(word)
    return sorted(forms)


def write_ortho_binary(words: list[str], out: Path) -> None:
    """Пишет отсортированный список слов во встроенный бинарный формат.

    Формат (little-endian, см. также src/ortho.rs):
      u32 magic "ORTO" | u32 version | u32 count | u32 stride
      u32 offsets[ceil(count/stride)] — позиции первых записей блоков
      записи в порядке сортировки: u8 prefix_len | u8 suffix_len | suffix

    Каждая запись хранит только байты, отличающие её от предыдущего слова;
    первая запись каждого блока — всегда целиком (prefix_len = 0), чтобы
    бинарный поиск по блокам читал первое слово без докатывания префикса.
    Поиск — бинарный по индексу + линейный проход по блоку из `stride` записей.
    """
    nblocks = (len(words) + ORTHO_STRIDE - 1) // ORTHO_STRIDE
    header_size = 16 + 4 * nblocks

    body = bytearray()
    offsets = []
    prev = b""
    for i, word in enumerate(words):
        block_start = i % ORTHO_STRIDE == 0
        if block_start:
            offsets.append(header_size + len(body))
            prev = b""
        raw = word.encode("utf-8")
        shared = 0
        for a, b in zip(raw, prev):
            if a != b:
                break
            shared += 1
        suffix = raw[shared:]
        if shared > 0xFF or len(suffix) > 0xFF:
            raise SystemExit(f"слишком длинное слово для формата: {word!r}")
        body += bytes((shared, len(suffix))) + suffix
        prev = raw

    with out.open("wb") as fh:
        fh.write(struct.pack("<4sIII", b"ORTO", 1, len(words), ORTHO_STRIDE))
        for off in offsets:
            fh.write(struct.pack("<I", off))
        fh.write(body)


def prepare_ortho(lang: str) -> int:
    print(f"[{lang} орфо]")
    aff, dic = download_hunspell(lang)
    words = expand_hunspell(lang, aff, dic)
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    out = OUT_DIR / f"{lang}_ortho.bin"
    write_ortho_binary(words, out)
    print(f"  форм: {len(words)}, записан {out.relative_to(ROOT)} ({out.stat().st_size} байт)")
    return len(words)


def main() -> None:
    for lang in LANGS:
        n = prepare(lang)
        if n == 0:
            raise SystemExit(f"пустой словарь для {lang}")
        m = prepare_ortho(lang)
        if m == 0:
            raise SystemExit(f"пустой орфо-словарь для {lang}")
    print("готово")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
