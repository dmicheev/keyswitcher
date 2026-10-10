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

Зависимости: только stdlib (urllib). Запуск: `make dicts` или `python3 tools/dict/prepare.py`.
Скачанные сырые файлы кэшируются в `tools/dict/cache/`.
"""

from __future__ import annotations

import re
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


def main() -> None:
    for lang in LANGS:
        n = prepare(lang)
        if n == 0:
            raise SystemExit(f"пустой словарь для {lang}")
    print("готово")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
