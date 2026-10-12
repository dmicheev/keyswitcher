# Makefile для keyswitcher (smartswitcher)
# Сборка, отладка и упаковка в самостоятельный .app-бандл для macOS.
#
# Подпись: TCC хранит разрешение «Доступность» (Accessibility) по кодовой
# подписи. Ad-hoc-подпись меняется при каждой пересборке — macOS считает
# бандл «другим» приложением и просит выдать разрешение заново. Чтобы
# разрешение переживало пересборки, один раз выполните `make cert`:
# бандл начнёт подписываться self-signed сертификатом (CODESIGN_ID) со
# стабильной TCC-идентичностью. Без сертификата используется ad-hoc.

APP_NAME    := Smartswitcher
BIN_NAME    := smartswitcher
BUNDLE_ID   := ru.dimka.smartswitcher
VERSION     := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)

# Имя self-signed сертификата подписи (создаётся один раз: make cert).
# Можно переопределить, например, на "Apple Development: ..." при желании.
CODESIGN_ID ?= smartswitcher-dev
# Если идентичность найдена в связке ключей — подписываем ею, иначе ad-hoc.
SIGN_ID := $(shell security find-identity -p codesigning -v 2>/dev/null | grep -qF '$(CODESIGN_ID)' && echo '$(CODESIGN_ID)' || echo '-')

RELEASE_BIN := target/release/$(BIN_NAME)
DEBUG_BIN   := target/debug/$(BIN_NAME)
BUNDLE_ROOT := build
APP_BUNDLE  := $(BUNDLE_ROOT)/$(APP_NAME).app

.PHONY: help build debug run lldb test fmt clippy check release bundle install uninstall run-bundle stop-bundle reset-tcc cert clean dicts

.DEFAULT_GOAL := help

help:
	@echo "keyswitcher ($(VERSION)) — цели:"
	@echo "  make build        - сборка debug-версии"
	@echo "  make run          - сборка и запуск debug-версии в консоли"
	@echo "  make lldb         - запуск debug-версии под отладчиком lldb"
	@echo "  make debug        - сборка debug + запуск (отладочный прогон)"
	@echo "  make test         - unit-тесты (cargo test)"
	@echo "  make dicts        - (пере)генерация словарей assets/lang/*.tsv (нужна сеть)"
	@echo "  make fmt          - форматирование (cargo fmt)"
	@echo "  make clippy       - линтер (cargo clippy)"
	@echo "  make check        - fmt --check + clippy + тесты"
	@echo "  make release      - релизная сборка (optimized)"
	@echo "  make cert         - ОДИН РАЗ: self-signed сертификат подписи ($(CODESIGN_ID))"
	@echo "  make bundle       - релиз + самостоятельный $(APP_NAME).app (без консоли)"
	@echo "  make install      - установка бандла в /Applications"
	@echo "  make reset-tcc    - сброс записи «Доступность» для $(BUNDLE_ID) (sudo)"
	@echo "  make run-bundle   - запуск установленного/локального бандла (open)"
	@echo "  make stop-bundle  - остановка запущенного агента"
	@echo "  make uninstall    - удаление из /Applications"
	@echo "  make clean        - cargo clean + удалить бандл"

build:
	cargo build

run: build
	./$(DEBUG_BIN)

lldb: build
	lldb -- $(DEBUG_BIN)

debug: build
	./$(DEBUG_BIN)

test:
	cargo test

# Частотные словари для режима печати: скачивание FrequencyWords и
# генерация assets/lang/{en,ru}.tsv (встраиваются в бинарник).
dicts:
	python3 tools/dict/prepare.py

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets

check: ; cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test

release:
	cargo build --release

# Самостоятельный .app-бандл: запускается через Finder/open без консоли.
# LSUIElement — агент без иконки в Dock. Конфиг и журнал живут в
# ~/.config/smartswitcher — в бандле только код и метаданные.
bundle: release
	@rm -rf "$(APP_BUNDLE)"
	@mkdir -p "$(APP_BUNDLE)/Contents/MacOS" "$(APP_BUNDLE)/Contents/Resources"
	@cp "$(RELEASE_BIN)" "$(APP_BUNDLE)/Contents/MacOS/$(BIN_NAME)"
	@printf 'APPL????' > "$(APP_BUNDLE)/Contents/PkgInfo"
	@printf '<?xml version="1.0" encoding="UTF-8"?>\n' > "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '<plist version="1.0">\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '<dict>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleName</key><string>$(APP_NAME)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleDisplayName</key><string>$(APP_NAME)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleIdentifier</key><string>$(BUNDLE_ID)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleExecutable</key><string>$(BIN_NAME)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundlePackageType</key><string>APPL</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleShortVersionString</key><string>$(VERSION)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleVersion</key><string>$(VERSION)</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>LSMinimumSystemVersion</key><string>10.13</string>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>LSUIElement</key><true/>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>NSHighResolutionCapable</key><true/>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>NSSupportsAutomaticTermination</key><false/>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '  <key>NSSupportsSuddenTermination</key><false/>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@printf '</dict>\n</plist>\n' >> "$(APP_BUNDLE)/Contents/Info.plist"
	@plutil -lint "$(APP_BUNDLE)/Contents/Info.plist"
	@echo "Подпись бандла: $(SIGN_ID)"
	@codesign --force --sign '$(SIGN_ID)' "$(APP_BUNDLE)"
	@echo "Готово: $(APP_BUNDLE) ($(VERSION))"

install: bundle
	@rm -rf "/Applications/$(APP_NAME).app"
	@cp -R "$(APP_BUNDLE)" /Applications/
	@echo "Установлено: /Applications/$(APP_NAME).app"
	@if [ "$(SIGN_ID)" = "-" ]; then \
		echo "ВНИМАНИЕ: бандл подписан ad-hoc — после каждой пересборки придётся заново"; \
		echo "выдавать «Доступность». Один раз выполните: make cert && make install."; \
	else \
		echo "Подпись стабильная ($(SIGN_ID)): выдайте «Доступность» один раз —"; \
		echo "последующие пересборки разрешение сохранят."; \
	fi
	@echo "Выдача разрешения: Системные настройки -> Конфиденциальность и безопасность -> Доступность."

# Одноразовое создание self-signed сертификата подписи (см. шапку файла).
cert:
	sh tools/codesign/make-cert.sh '$(CODESIGN_ID)'

# Сброс устаревшей TCC-записи «Доступность» (например, после перехода
# с ad-hoc на стабильную подпись). Нужен пароль администратора.
reset-tcc:
	@echo "Сброс записи «Доступность» для $(BUNDLE_ID) (нужен пароль администратора)..."
	@sudo tccutil reset Accessibility $(BUNDLE_ID) \
		|| echo "Не получилось — удалите $(APP_NAME) из списка «Доступность» вручную."

uninstall:
	@rm -rf "/Applications/$(APP_NAME).app"

run-bundle:
	@if [ -d "/Applications/$(APP_NAME).app" ]; then \
		open "/Applications/$(APP_NAME).app"; \
	else \
		open "$(APP_BUNDLE)"; \
	fi

stop-bundle:
	@pkill -x "$(BIN_NAME)" || true

clean:
	cargo clean
	rm -rf "$(BUNDLE_ROOT)"
