#!/bin/sh
# make-cert.sh — одноразовое создание self-signed сертификата для подписи
# Smartswitcher.app при локальной сборке.
#
# Зачем: TCC хранит разрешение «Доступность» (Accessibility) по кодовой
# подписи приложения. Ad-hoc-подпись меняется при каждой пересборке, поэтому
# галочку приходится выдавать заново. Стабильный self-signed сертификат
# делает требование подписи постоянным, и разрешение переживает пересборки.
#
# Использование: tools/codesign/make-cert.sh [имя-сертификата]
# По умолчанию — smartswitcher-dev (как CODESIGN_ID в Makefile).

set -eu

CERT_NAME="${1:-smartswitcher-dev}"
DAYS=3650
WORK="$(mktemp -d "${TMPDIR:-/tmp}/smartswitcher-cert.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

if security find-identity -p codesigning -v 2>/dev/null | grep -qF "$CERT_NAME"; then
	echo "Сертификат \"$CERT_NAME\" уже есть в связке ключей — пропускаю создание."
	exit 0
fi

cat > "$WORK/codesign.cnf" <<EOF
[req]
distinguished_name = dn
prompt = no
x509_extensions = v3_codesign

[dn]
CN = $CERT_NAME

[v3_codesign]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = codeSigning
EOF

echo "Создаю self-signed сертификат \"$CERT_NAME\" (срок $DAYS дней)..."
openssl req -x509 -newkey rsa:2048 -sha256 -days "$DAYS" -nodes \
	-keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
	-config "$WORK/codesign.cnf" 2>/dev/null

# PKCS#12: OpenSSL 3.x по умолчанию пишет AES-256/PBKDF2-MAC, который
# «security import» на macOS не переваривает (MAC verification failed).
# Для совместимости нужна связка SHA1/3DES: у OpenSSL 3 — флаг -legacy,
# LibreSSL параметров -legacy не знает, но её дефолт уже совместим.
if openssl pkcs12 -help 2>&1 | grep -q -- -legacy; then
	LEGACY="-legacy"
else
	LEGACY=""
fi
# shellcheck disable=SC2086
openssl pkcs12 -export -passout pass:codesign \
	-inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
	-out "$WORK/identity.p12" \
	-macalg sha1 -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES $LEGACY

echo "Импортирую идентичность в связку ключей (login)..."
security import "$WORK/identity.p12" \
	-k "$HOME/Library/Keychains/login.keychain-db" \
	-P codesign -T /usr/bin/codesign

# Без trust-записи self-signed идентичность помечена CSSMERR_TP_NOT_TRUSTED
# и codesign её не возьмёт. Доверие для code signing — в user domain
# (без sudo); macOS может показать диалог подтверждения — согласиться.
echo "Включаю доверие сертификата для code signing..."
security add-trusted-cert -p codeSign \
	-k "$HOME/Library/Keychains/login.keychain-db" "$WORK/cert.pem"

if security find-identity -p codesigning -v 2>/dev/null | grep -qF "$CERT_NAME"; then
	echo "Готово: идентичность \"$CERT_NAME\" доступна для codesign."
	echo "Дальше: make install — выдайте «Доступность» один раз, пересборки её сохранят."
else
	echo "ОШИБКА: идентичность \"$CERT_NAME\" не найдена после импорта." >&2
	exit 1
fi
