#!/bin/sh
# Tune Bridge endpoint — установка на macOS:
# бинарь в /usr/local/bin + LaunchAgent (автозапуск при логине, рестарт при падении).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=/usr/local/bin/tune-bridge
CONF_DIR="$HOME/Library/Application Support/tune-bridge"
CONF="$CONF_DIR/endpoint.toml"
PLIST="$HOME/Library/LaunchAgents/com.oaat.endpoint.plist"

[ -f "$HERE/tune-bridge" ] || { echo "tune-bridge binary not found next to install.sh"; exit 1; }

install -d "$CONF_DIR"
if [ ! -f "$CONF" ]; then
  cat > "$CONF" <<EOF
[endpoint]
name = "$(hostname -s)"
port = 9740

[web_ui]
enabled = true
port = 9741
bind = "0.0.0.0"
# control_token генерируется автоматически при первом запуске
# (файл endpoint.toml.token рядом с конфигом) и подхватывается
# сервером SoundVault через mDNS — вручную задавать не нужно.
EOF
  echo "created $CONF"
fi

if [ -w /usr/local/bin ]; then
  install -m 755 "$HERE/tune-bridge" "$BIN"
else
  sudo install -m 755 "$HERE/tune-bridge" "$BIN"
fi

install -d "$HOME/Library/LaunchAgents"
sed -e "s|@CONF@|$CONF|" "$HERE/com.oaat.endpoint.plist.template" > "$PLIST"
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "Tune Bridge установлен и запущен (LaunchAgent: com.oaat.endpoint)."
echo "  конфиг: $CONF"
echo "  лог:    /tmp/tune-bridge.log"
echo "  стоп:   launchctl unload $PLIST"
