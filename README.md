# Cashy Updater – GUI für cachy-update

Eine Linux-Desktop-Anwendung in Rust und [Slint](https://slint.dev) für
**`cachy-update`**, den Update-Assistenten von CachyOS (ein Fork von
[arch-update](https://github.com/Antiz96/arch-update)).

Die gesamte Update-Logik bleibt in `cachy-update`. Die GUI ruft sie mit festen
Argumenten auf, liest die Statusdateien, die `cachy-update` selbst schreibt, und
zeigt den interaktiven Update-Lauf (Bestätigungen, sudo-Passwort, Neustart-Abfrage …)
in der Oberfläche an. Es gibt keine eigenen Update-Quellen, Endpunkte oder
Installationswege.

---

## 1. Bestandsaufnahme (Ausgangslage)

Das Projektverzeichnis war zu Beginn leer. Als „bestehende Update-Logik“ wurde mit
dem Auftraggeber das installierte Paket **`cachy-update 4.4.2-1`** festgelegt.

| Aspekt | Befund in `cachy-update` |
|---|---|
| Sprache | Bash (`/usr/bin/arch-update`, Bibliotheken in `/usr/share/arch-update/lib/*.sh`). `cachy-update` ist ein Symlink darauf, intern gilt immer `name="arch-update"`. |
| Einstiegspunkte | `arch-update` (Vollupdate), `-d/--devel`, `-c/--check [--enable]`, `-l/--list`, `-n/--news`, `-s/--services`, `--gen-config`, `--show-config`, `--edit-config`, `--tray [--enable]`, `-V` |
| Update-Prüfung | `lib/check.sh`: `checkupdates` (temporäre DB), `<aur-helper> -Qua`, `flatpak remote-ls --updates`. Ergebnis landet in `$XDG_STATE_HOME/arch-update/last_updates_check_{packages,aur,flatpak}` und `last_updates_check_time`. **Fehler von `checkupdates` stehen nur auf stderr, der Exit-Code bleibt 0.** |
| Download/Prüfung/Installation | `lib/update.sh`: `<su_cmd> pacman -Syu`, danach AUR-Helper `-Syu`, `flatpak update`. Signaturen und Prüfsummen prüft pacman (Keyring) – arch-update hat dafür keine eigene Logik. |
| Ablauf Vollupdate | `lib/full_upgrade.sh`: Lock `/tmp/arch-update.lock` (flock) → Paketliste + „Proceed with update? [Y/n]“ → Arch-News → Update → verwaiste Pakete → Flatpak-Reste → Paket-Cache → Kernel-Neustart-Prüfung → Dienst-Neustarts → „Press enter to quit“ |
| Rechteerhöhung | `sudo`, `sudo-rs`, `doas` oder `run0` (automatisch oder `PrivilegeElevationCommand`), interaktiv im Terminal |
| Konfiguration | `$XDG_CONFIG_HOME/arch-update/arch-update.conf`, wird per `grep -E` gelesen (`lib/config.sh`, Manpage `arch-update.conf(5)`) |
| Automatische Prüfung | systemd-User-Units `arch-update.timer`/`.service` (`arch-update --check`) |
| Exit-Codes | 0 ok, 2 keine Rechteerhöhung, 4 vom Benutzer abgebrochen, 5 pacman-Fehler, 6 Neustart-Fehler, 7 keine Updates (`--list`), 11 Dienst-Neustart-Fehler, 17 andere Instanz läuft, … |
| Logging | keines (nur Terminalausgabe) |
| Tray | eigenes Binary `/usr/lib/arch-update/arch-update-tray` (GLib), startet Updates über `arch-update.desktop` (`Terminal=true`) |

**Nicht vorhanden** (und deshalb in der GUI auch nicht nachgebaut):

- eine einzelne „installierte/verfügbare Version“ – `cachy-update` arbeitet mit einer Liste ausstehender Paket-Updates;
- eine maschinenlesbare Fortschritts-API – Fortschritt wird aus der pacman-Ausgabe (`(n/m)`-Zähler) gelesen und nur dann als Prozentwert angezeigt;
- Release Notes – angeboten werden die Arch-News, die `arch-update --news` liefert;
- ein Abbruch während der pacman-Transaktion – ein Abbruch ist nur vor Beginn der Paketänderungen möglich.

## 2. Architektur

```text
src/
├── main.rs                 CLI (--version/--help) → app::run()
├── app.rs                  Lebenszyklus: Pfade, Einstellungen, Logging, Fenster, Beenden
├── lib.rs
├── error.rs                Fehlerkategorien + Zuordnung von pacman/sudo-Meldungen
├── logging.rs              tracing-Layer → Ringpuffer (GUI) + Logdatei, Filter, Export
├── paths.rs                XDG-Pfade (bildet die Pfadlogik von arch-update exakt nach)
├── state.rs                AppState (exklusive Vorgänge), UpdateState, letztes Ergebnis
├── config/
│   ├── settings.rs         eigene GUI-Einstellungen (TOML), Validierung, Sicherung defekter Dateien
│   └── arch_update_conf.rs zeilengenaues Lesen/Ändern der vorhandenen arch-update.conf
├── updater/                Adapter zu cachy-update
│   ├── service.rs          Backend-Trait + UpdaterService (Aufrufe, Statusdateien)
│   ├── runner.rs           Prozessaufrufe ohne Shell, Abbruch per Prozessgruppe
│   ├── session.rs          interaktiver Vollupdate-Lauf im PTY
│   ├── progress.rs         ANSI-/\r-Verarbeitung, pacman-Zähler
│   ├── status.rs           Phasen, 5 Schritte, Abfrage-Erkennung, Ergebnis aus Exit-Code
│   ├── state_files.rs      Parser für die Statusdateien von arch-update
│   └── system.rs           Kernel-Neustart-Prüfung, pacman.log, Lock-Prüfung
└── ui/
    ├── controller.rs       UI-unabhängige Steuerlogik (vollständig testbar)
    ├── bridge.rs           Slint-Anbindung, Hintergrund-Threads, Schließen-Schutz
    └── theme.rs
ui/                         Slint-Oberfläche (main, theme, bridge, components/, pages/)
tests/                      Integrations- und GUI-Tests, Test-Doubles in tests/fixtures/bin
packaging/                  Desktop-Datei, Icon, Install-Skript, PKGBUILD
```

**Wichtige Entscheidungen**

- **Slint (Fluent-Stil)**: Das Projekt hatte noch kein GUI-Framework, und der Auftrag nannte Slint als bevorzugte Wahl. Die Oberfläche ist deklarativ, es gibt keine GTK/Qt-Laufzeitabhängigkeit, und Wayland sowie X11 laufen über winit.
- **PTY statt Nachbau**: Der Vollupdate-Lauf ist durchgehend interaktiv und nutzt `sudo`/`doas`/`run0`. Deshalb läuft das unveränderte `cachy-update` in einem Pseudo-Terminal (`portable-pty`). Rechteerhöhung, Signatur- und Prüfsummenprüfung durch pacman sowie alle Nacharbeiten bleiben exakt erhalten. Die GUI läuft nie mit Root-Rechten.
- **C-Locale** (`LC_ALL=C.UTF-8`) für den Update-Lauf: Phasen und Abfragen lassen sich so sprachunabhängig erkennen. Die Prüfung (`--check`) läuft mit der Benutzer-Locale, damit Desktop-Benachrichtigungen lokalisiert bleiben.
- **Kein Tokio**: Es gibt nur wenige, blockierende Prozessaufrufe. Dafür reichen `std::thread` und `slint::invoke_from_event_loop`. Der Zustand wird ausschließlich im GUI-Thread verändert; der einzige Mutex schützt den Log-Puffer.
- **Controller ohne Slint**: Die gesamte Logik (Zustandsübergänge, Sperren, Abbruchregeln, Dashboard-Werte) ist ohne Display testbar.

## 3. Funktionen

| Bereich | Umsetzung |
|---|---|
| **Übersicht** | Status, Version von `arch-update` und dem besitzenden Paket (`pacman -Qo`), Anzahl Updates je Quelle, letzte Prüfung (Statusdatei), letzte Installation über die App, letzte abgeschlossene pacman-Transaktion (`/var/log/pacman.log`), Neustart erforderlich (gleiche Prüfung wie `kernel_reboot.sh`), Timer-Status, Arch-News (`arch-update --news`). Unbekanntes wird als „Unbekannt“/„Noch nie“ angezeigt. |
| **Nach Updates suchen** | `arch-update --check` im Hintergrund, abbrechbar. Meldungen auf stderr führen zu „Prüfung unvollständig“, niemals zu „System ist aktuell“. |
| **Update installieren** | Nur aktiv, wenn arch-update Updates gemeldet hat, kein anderer Vorgang läuft und `/tmp/arch-update.lock` frei ist. Startet `arch-update` (optional `--devel`) im PTY. |
| **Update-Ansicht** | 5 Schritte (Prüfung, Download, Integritätsprüfung, Installation, Abschluss). Nicht beobachtete Schritte werden als „nicht beobachtet“ markiert. Fortschrittsbalken nur bei `(n/m)`-Zählern, sonst unbestimmt. Ausgabe live, Eingabefeld mit Ja/Nein/Enter-Schaltflächen, Passwortfeld (maskiert). |
| **Abbruch** | Nur vor Paketänderungen möglich: An der Bestätigungsabfrage sendet die GUI „n“ (arch-update bricht selbst ab), davor oder während der News wird die Prozessgruppe per SIGTERM beendet. Danach ist der Abbruch gesperrt und ein Hinweis wird angezeigt. |
| **Fenster schließen** | Bei laufender Prüfung wird sie abgebrochen. Bei laufendem Update erscheint ein Dialog: „Abbrechen und beenden“ nur, solange noch keine Pakete geändert wurden, sonst bleibt das Fenster offen. |
| **Ergebnis** | Wird aus Exit-Code und Ausgabe bestimmt. Erfolg wird nur gemeldet, wenn arch-update „The update has been applied“ ausgibt. Fehler werden kategorisiert (Berechtigung, Integrität, Netzwerk, Download, Installation, andere Instanz …) und mit aufklappbaren technischen Details angezeigt. |
| **Protokoll** | Laufende Anzeige (Ringpuffer, standardmäßig 5000 Einträge, Anzeige in Blöcken zu 500 mit „Ältere laden“), Filter nach Level, Volltextsuche, Auswahl kopieren, Export (`$XDG_STATE_HOME/cashy-updater-gui/exports/`), „Anzeige leeren“ (die Logdatei bleibt unverändert). Logdatei: `$XDG_STATE_HOME/cashy-updater-gui/logs/cashy-updater-gui.log` (Rotation bei 5 MiB). |
| **Einstellungen** | Theme (dunkel/hell), Prüfung beim Start, Status nach dem Update neu einlesen, `--devel`, Log-Optionen. Gespeichert in `$XDG_CONFIG_HOME/cashy-updater-gui/settings.toml`. Eine defekte Datei wird gesichert und durch Standardwerte ersetzt. |
| **Automatische Prüfung** | Aktivieren über `arch-update --check --enable` (vorhandener Mechanismus), Deaktivieren über `systemctl --user disable --now arch-update.timer`. |
| **cachy-update-Konfiguration** | Alle in `arch-update.conf(5)` dokumentierten Optionen außer `DiffProg` (frei ausführbarer Befehl; die pacnew-Verarbeitung ist in cachy-update deaktiviert). Es werden nur Werte akzeptiert, die auch `config.sh` akzeptiert. Kommentare und Reihenfolge bleiben erhalten. Fehlt die Datei, wird sie über `arch-update --gen-config` erzeugt. |
| **Im Terminal** | Startet `arch-update.desktop` über `gio launch` (wie das Tray-Applet). Ein externer Lauf wird über das Lock erkannt. |
| **Diagnose** | Versionen, Pfade, Sitzungstyp. Lässt sich kopieren. |
| **Bedienung** | Tab-Navigation mit sichtbarem Fokusring, Enter/Leertaste auf Schaltflächen, `Strg+1…4` wechselt die Seite, `F5` startet die Prüfung. Barrierefreiheits-Labels sind gesetzt. |

## 4. Sicherheit

- Alle Prozesse werden mit strukturierten Argumentlisten gestartet; es gibt keine Shell-Interpolation (`std::process::Command`, `portable-pty::CommandBuilder`).
- Keine eigenen Root-Rechte: Erhöhte Rechte holt sich ausschließlich `cachy-update` über den konfigurierten Mechanismus.
- Signatur-, Schlüssel- und Prüfsummenprüfung durch pacman bleiben unverändert. Ein Integritätsfehler führt immer zu „fehlgeschlagen“.
- Das Passwort geht direkt in das PTY. Es wird nicht gespeichert und nicht protokolliert. Im Protokoll erscheinen nur kurze Antworten (`y`, `n`, Ziffern) im Klartext; andere Texteingaben werden nur als „nicht protokolliert“ vermerkt.
- Parallele oder wiederholte Läufe werden doppelt verhindert: durch einen exklusiven Vorgang in der GUI und durch die Prüfung von `/tmp/arch-update.lock`.
- Keine automatische Beantwortung von Abfragen – mit einer Ausnahme: das abschließende „Press enter to quit“, das nur ein Terminalfenster offen hält.

## 5. Build und Start

Getestet auf **CachyOS (Arch-basiert)** mit Rust 1.99.0. Andere Distributionen sind nicht getestet. `cachy-update` bzw. `arch-update` muss installiert sein.

```bash
# Build-Abhängigkeiten (Arch/CachyOS)
sudo pacman -S --needed rust fontconfig freetype2 libxkbcommon wayland libglvnd \
                        libx11 libxcursor libxi libxkbcommon-x11

cargo build --release
./target/release/cashy-updater-gui

# Installation nach ~/.local (ohne Root) bzw. Entfernen
packaging/install-local.sh
packaging/install-local.sh --uninstall

# Arch-Paket aus dem lokalen Quellbaum
cd packaging/arch && makepkg -si
```

> **Hinweis zur Build-Geschwindigkeit:** Liegt der Quellbaum auf einem NTFS-Laufwerk (ntfs-3g/FUSE), sind Linken und Tests stark I/O-gebunden. Abhilfe: `export CARGO_TARGET_DIR=~/.cache/cashy-updater-gui-target` (natives Dateisystem).

**Laufzeitbibliotheken** (geprüft mit `ldd` bzw. den von winit/Slint dynamisch geladenen Bibliotheken): `fontconfig`, `freetype2`, `libxkbcommon`, `wayland` (`libwayland-client`, `libwayland-egl`), `libglvnd`/`mesa` (EGL/OpenGL), unter X11 außerdem `libx11`, `libxcursor`, `libxi`, `libxkbcommon-x11`. Optional: `xdg-utils` (Ordner und Arch-News öffnen) und `gio` (aus `glib2`, ohnehin eine Abhängigkeit von cachy-update).

Kann das Fenster nicht erstellt werden (keine Wayland-/X11-Sitzung oder fehlende Bibliotheken), beendet sich das Programm mit einer verständlichen Meldung und Exit-Code 1.

## 6. Tests

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

| Suite | Inhalt |
|---|---|
| Unit-Tests (`src/**`) | Fehlerzuordnung, XDG-Pfade, Log-Puffer/Filter, Einstellungen, arch-update.conf-Editor, ANSI/`\r`-Parser, pacman-Zähler, Abfrage-Erkennung, Phasen/Schritte/Exit-Codes, Fortschritts-Grenzwerte, Statusdateien, pacman.log, Lock-Erkennung, Prozess-Abbruch, UTF-8 über Chunk-Grenzen |
| `tests/updater_tests.rs` | Adapter gegen das Test-Double `tests/fixtures/bin/cachy-update`: Version/Paket, fehlendes Tool, Prüfung, unvollständige Prüfung, Abbruch, News, Timer, Neustart-Erkennung, Startparameter, `--gen-config` |
| `tests/integration_tests.rs` | Controller + Service + **echte PTY-Sitzung**: erfolgreicher Lauf, Neustart-Abfrage, Integritätsfehler, falsches Passwort, Abbruch an der Bestätigung, Abbruchsperre während pacman, parallele Vorgänge, fremdes Lock, aktuelles System, News, Logdatei und Export |
| `tests/config_tests.rs` | fehlende, defekte und typfalsche Einstellungsdateien, Wertebereiche, atomares Speichern, conf-Roundtrip mit dem echten Beispiel |
| `tests/gui_tests.rs` | **echtes Hauptfenster** im Slint-Test-Backend, bedient über Accessibility-Aktionen: Prüfung, Installation, Schließen-Dialog, Ja/Passwort/Ja, Abbruchsperre, Ergebnis und Schritte, Protokoll und Filter, Theme-Wechsel und Speichern, Schließen im Leerlauf |

Die Test-Doubles (`tests/fixtures/bin/`) bilden Ausgabe, Abfragen, Statusdateien und Exit-Codes von arch-update 4.4.x nach. **Kein Test verwendet echte Paketquellen, `sudo` oder pacman-Transaktionen.**

**Manuell zu prüfen** (in der Testumgebung nicht automatisierbar):

- ein echter Update-Lauf mit `sudo` bzw. `run0` (Polkit-Agent) und echten Paketen;
- AUR-Updates mit `paru`/`yay`: Deren eigene Menüs und Pager (z. B. die PKGBUILD-Prüfung) laufen über das Eingabefeld der GUI. Für komfortable Reviews ist „Im Terminal“ gedacht;
- „Im Terminal“ (`gio launch arch-update.desktop`) mit dem konfigurierten Terminal-Emulator;
- Zwischenablage (`arboard`, Wayland data-control bzw. X11) und `xdg-open`;
- Darstellung mit der System-Akzentfarbe: Standard-Widgets wie Schalter, Checkboxen und Eingabefelder übernehmen die Akzentfarbe des Desktops.

## 7. Bekannte Einschränkungen

- Phasen- und Abfrage-Erkennung beruhen auf den (C-Locale-)Texten von arch-update 4.4.x und pacman. Ändern sich diese Texte, kann eine Phase als „nicht beobachtet“ erscheinen. Das Ergebnis selbst stützt sich zusätzlich auf den Exit-Code. Das Eingabefeld ist während des Laufs immer verfügbar, auch wenn eine Abfrage nicht erkannt wird.
- Der Download-Fortschritt bei parallelen pacman-Downloads wird über die „Total (n/m)“-Zeile angenähert.
- Flatpak-Einträge werden so angezeigt, wie arch-update sie speichert (Name und Version in einer Zeile).
- „Letzte Installation“ bezieht sich auf Läufe über diese App. Für alle anderen Läufe zeigt die Übersicht die letzte pacman-Transaktion und das Datum des letzten Laufs laut arch-update.
- Das Release-Binary ist mit den Slint-Standardfeatures etwa 31 MB groß.
- Die Lizenz des Projekts ist noch festzulegen (`license=('unknown')` im PKGBUILD). arch-update selbst steht unter GPL-3.0-or-later.
