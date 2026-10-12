#!/bin/bash
# Installiert bzw. entfernt Cashy Updater (Binary, Desktop-Datei, Icon).
#
#   packaging/install-local.sh              → ~/.local (ohne Root)
#   packaging/install-local.sh --uninstall  → entfernt die Dateien wieder
#   PREFIX=/usr/local sudo -E packaging/install-local.sh   → systemweit
#
# Voraussetzung: `cargo build --release` wurde ausgeführt.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix="${PREFIX:-${HOME}/.local}"
bin="${prefix}/bin/cashy-updater-gui"
desktop="${prefix}/share/applications/cashy-updater-gui.desktop"
icon="${prefix}/share/icons/hicolor/scalable/apps/cashy-updater-gui.svg"

refresh_caches() {
	command -v update-desktop-database >/dev/null && update-desktop-database -q "${prefix}/share/applications" 2>/dev/null || true
	command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "${prefix}/share/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
	rm -fv -- "${bin}" "${desktop}" "${icon}"
	refresh_caches
	exit 0
fi

if [ ! -x "${root}/target/release/cashy-updater-gui" ]; then
	echo "Fehler: ${root}/target/release/cashy-updater-gui fehlt – zuerst 'cargo build --release' ausführen." >&2
	exit 1
fi

install -Dm755 "${root}/target/release/cashy-updater-gui" "${bin}"
install -Dm644 "${root}/packaging/desktop/cashy-updater-gui.desktop" "${desktop}"
install -Dm644 "${root}/packaging/desktop/cashy-updater-gui.svg" "${icon}"
refresh_caches
echo "Installiert nach ${prefix}"
case ":${PATH}:" in
	*":${prefix}/bin:"*) ;;
	*) echo "Hinweis: ${prefix}/bin ist nicht im PATH; die Desktop-Datei startet 'cashy-updater-gui' über den PATH." ;;
esac
