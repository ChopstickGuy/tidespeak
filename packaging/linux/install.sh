#!/bin/sh
# Installiert Tidespeak für den aktuellen Benutzer (ohne root)
set -e
cd "$(dirname "$0")"
mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/scalable/apps"
install -m 755 tidespeak tidespeak-server "$HOME/.local/bin/"
install -m 644 tidespeak.desktop "$HOME/.local/share/applications/"
install -m 644 tidespeak.svg "$HOME/.local/share/icons/hicolor/scalable/apps/"
update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
echo "Tidespeak ist installiert – im App-Menü oder mit 'tidespeak' starten."
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "Hinweis: ~/.local/bin ist nicht im PATH.";; esac
