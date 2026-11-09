#!/usr/bin/env bash
# Сборка образа Raspberry Pi OS Lite (arm64) с предустановленным SoundVault-endpoint'ом.
# Результат — .img.xz, который прошивается Balena Etcher / Raspberry Pi Imager.
#
# Требования: libguestfs-tools (virt-customize), curl, xz.
# Использование:
#   DEB=path/to/tune-bridge_*_arm64.deb scripts/build-pi-image.sh [out.img.xz]
#
# Настройка Wi-Fi/SSH — штатной кастомизацией Raspberry Pi Imager (работает для
# любого RPi OS-образа) или файлами firstrun.sh/userconf.txt в boot-секции.
# I2S DAC-шляпы: раскомментируйте dtoverlay в config.txt (boot-секция видна с
# любой ОС). Pi2Design 502DAC (PCM5122) = dtoverlay=hifiberry-dacplus.
set -euo pipefail

DEB="${DEB:?нужен DEB=путь к arm64 .deb пакета tune-bridge}"
OUT="${1:-soundvault-pi-arm64.img.xz}"
BASE_URL="${BASE_URL:-https://downloads.raspberrypi.com/raspios_lite_arm64_latest}"

[ -f "$DEB" ] || { echo "deb не найден: $DEB"; exit 1; }
command -v virt-customize >/dev/null || { echo "нужен libguestfs-tools"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== Качаю Raspberry Pi OS Lite arm64"
curl -fL --retry 3 -o "$WORK/base.img.xz" "$BASE_URL"
xz -d "$WORK/base.img.xz"
IMG="$WORK/base.img"

echo "== Кастомизирую образ (virt-customize)"
cp "$DEB" "$WORK/tune-bridge.deb"
virt-customize -a "$IMG" \
  --hostname soundvault-pi \
  --network \
  --copy-in "$WORK/tune-bridge.deb:/tmp/" \
  --run-command "dpkg -i /tmp/tune-bridge.deb" \
  --run-command "apt-get update -qq && (apt-get install -y --no-install-recommends libasound2t64 || apt-get install -y --no-install-recommends libasound2) || true" \
  --write /etc/oaat/endpoint.toml:"$(printf '[endpoint]\nname = "SoundVault Pi"\nport = 9740\n\n[web_ui]\nenabled = true\nport = 9741\nbind = "0.0.0.0"\n# control_token генерируется автоматически при первом запуске\n# и подхватывается сервером SoundVault через mDNS TXT.\n')" \
  --run-command "printf '\n# --- SoundVault: I2S DAC hat ---\n# Pi2Design 502DAC (PCM5122), HiFiBerry DAC+/Pro/Amp2:\n#dtoverlay=hifiberry-dacplus\n# Рекомендуется при использовании DAC-шляпы:\n#dtparam=audio=off\n' >> /boot/firmware/config.txt" \
  --run-command "systemctl enable oaat-endpoint.service" \
  --run-command "rm -f /etc/oaat/endpoint.toml.token" \
  --firstboot-command "systemctl restart oaat-endpoint.service"

echo "== Сжимаю"
xz -9e -T0 "$IMG"
mv "$IMG.xz" "$OUT"
sha256sum "$OUT" > "$OUT.sha256"
ls -lh "$OUT" "$OUT.sha256"
echo "== Готово: $OUT"
