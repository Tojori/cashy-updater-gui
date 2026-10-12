//! Interaktive arch-update-Sitzung in einem Pseudo-Terminal (PTY).
//!
//! arch-update ist ein interaktives Terminalprogramm (Bestätigungen, sudo-
//! Passwort, News-Auswahl, Neustart-Abfrage, ...). Damit sämtliche Logik –
//! inklusive Rechteerhöhung und der Signatur-/Prüfsummenprüfung durch pacman –
//! unverändert bleibt, wird der Prozess in einem PTY gestartet. Die GUI liest
//! die Ausgabe und schreibt Benutzereingaben in das PTY.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::thread;

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Startparameter einer Sitzung.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Zusätzliche/überschriebene Umgebungsvariablen.
    pub env: Vec<(String, String)>,
    /// Zu entfernende Umgebungsvariablen.
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

/// Ereignisse aus dem Lese-Thread (in Reihenfolge; `Exited` kommt zuletzt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Output(String),
    Exited(Option<i32>),
}

/// Laufende Sitzung. Beim Drop wird der Prozess NICHT beendet – das
/// Schließen der GUI während eines Updates wird an anderer Stelle verhindert.
pub struct UpgradeSession {
    writer: Box<dyn Write + Send>,
    pid: Option<u32>,
    _master: Box<dyn MasterPty + Send>,
}

impl std::fmt::Debug for UpgradeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeSession")
            .field("pid", &self.pid)
            .finish()
    }
}

/// Dekodiert UTF-8 über Chunk-Grenzen hinweg.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    out.push_str(s);
                    self.pending.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&self.pending[..valid]).expect("valid prefix"),
                    );
                    match e.error_len() {
                        // Unvollständige Sequenz am Ende: auf weitere Bytes warten.
                        None => {
                            self.pending.drain(..valid);
                            return out;
                        }
                        Some(len) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            self.pending.drain(..valid + len);
                        }
                    }
                }
            }
        }
    }
}

impl UpgradeSession {
    /// Startet die Sitzung. `sink` wird aus einem Hintergrund-Thread aufgerufen.
    pub fn start<F>(spec: &SessionSpec, mut sink: F) -> io::Result<Self>
    where
        F: FnMut(SessionEvent) + Send + 'static,
    {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }

        let mut child = pair.slave.spawn_command(cmd).map_err(to_io)?;
        // Slave-Seite schließen, damit der Leser EOF erhält, sobald der Prozess endet.
        drop(pair.slave);
        let pid = child.process_id();
        let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer = pair.master.take_writer().map_err(to_io)?;

        thread::Builder::new()
            .name("arch-update-pty".into())
            .spawn(move || {
                let mut decoder = Utf8Decoder::default();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = decoder.decode(&buf[..n]);
                            if !text.is_empty() {
                                sink(SessionEvent::Output(text));
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // EIO: Slave geschlossen (Prozess beendet).
                        Err(_) => break,
                    }
                }
                let code = match child.wait() {
                    Ok(status) if status.signal().is_none() => Some(status.exit_code() as i32),
                    _ => None,
                };
                sink(SessionEvent::Exited(code));
            })?;

        Ok(Self {
            writer,
            pid,
            _master: pair.master,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Sendet eine Zeile (mit Zeilenende) an den Prozess.
    pub fn send_line(&mut self, text: &str) -> io::Result<()> {
        // Steuerzeichen außer Tab werden nicht weitergegeben.
        let clean: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .collect();
        self.writer.write_all(clean.as_bytes())?;
        self.writer.write_all(b"\r")?;
        self.writer.flush()
    }

    /// Beendet die Prozessgruppe der Sitzung (SIGTERM). Nur in sicheren Phasen verwenden.
    pub fn terminate(&mut self) -> io::Result<()> {
        let pid = self
            .pid
            .ok_or_else(|| io::Error::other("Prozess-ID unbekannt"))?
            as libc::pid_t;
        // portable-pty startet das Kind per setsid(): PID == Prozessgruppe.
        // SAFETY: einfacher Systemaufruf mit gültigen Argumenten.
        let rc = unsafe { libc::kill(-pid, libc::SIGTERM) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// portable-pty liefert `anyhow::Error`; Umwandlung in `io::Error`.
fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decoder_handles_split_sequences() {
        let mut d = Utf8Decoder::default();
        let bytes = "Prüfung ✓".as_bytes();
        let (a, b) = bytes.split_at(3); // mitten im 'ü'
        let mut s = d.decode(a);
        s.push_str(&d.decode(b));
        assert_eq!(s, "Prüfung ✓");
        assert_eq!(d.decode(&[0xff, b'a']), "\u{fffd}a");
    }
}
