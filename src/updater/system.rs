//! Rein lesende Systemabfragen, die arch-update intern ebenfalls verwendet.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::path::Path;

use chrono::{DateTime, Local};

/// Laufende Kernel-Version (entspricht `uname -r`).
pub fn running_kernel(osrelease: &Path) -> Option<String> {
    fs::read_to_string(osrelease)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Logik aus `lib/kernel_reboot.sh`: Fehlt `/usr/lib/modules/$(uname -r)/vmlinuz`,
/// wurde der laufende Kernel ersetzt und ein Neustart steht aus.
pub fn kernel_reboot_pending(modules_root: &Path, release: &str) -> bool {
    !modules_root.join(release).join("vmlinuz").is_file()
}

/// Prüft (wie `full_upgrade.sh` mit `flock -n`), ob eine arch-update-Instanz
/// die Sperrdatei hält. Die Sperre wird sofort wieder freigegeben.
pub fn lock_is_held(lock_file: &Path) -> bool {
    let Ok(file) = File::open(lock_file) else {
        return false;
    };
    let fd = file.as_raw_fd();
    // SAFETY: gültiger Dateideskriptor für die Lebensdauer von `file`.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // SAFETY: wie oben.
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        false
    } else {
        io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
    }
}

/// Liest höchstens die letzten `max_bytes` einer Datei.
fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Erste (ggf. abgeschnittene) Zeile verwerfen, wenn nicht ab Dateianfang gelesen.
    Ok(if start > 0 {
        text.split_once('\n')
            .map(|(_, r)| r.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

/// Zeitpunkt der letzten abgeschlossenen pacman-Transaktion aus `pacman.log`
/// (`[ALPM] transaction completed`). Liest nur das Dateiende, um die GUI
/// bei großen Logdateien nicht zu belasten.
pub fn last_completed_transaction(pacman_log: &Path) -> Option<DateTime<Local>> {
    let text = read_tail(pacman_log, 1024 * 1024).ok()?;
    let line = text
        .lines()
        .rev()
        .find(|l| l.contains("[ALPM] transaction completed"))?;
    let ts = line.strip_prefix('[')?.split(']').next()?;
    DateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%z")
        .ok()
        .map(|d| d.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detects_missing_kernel_image() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("6.1.0")).unwrap();
        fs::write(dir.path().join("6.1.0/vmlinuz"), b"x").unwrap();
        assert!(!kernel_reboot_pending(dir.path(), "6.1.0"));
        assert!(kernel_reboot_pending(dir.path(), "6.2.0"));
    }

    #[test]
    fn parses_last_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pacman.log");
        let mut f = File::create(&p).unwrap();
        writeln!(f, "[2026-10-11T09:00:00+0200] [ALPM] transaction completed").unwrap();
        writeln!(f, "[2026-10-12T10:42:01+0200] [ALPM] transaction started").unwrap();
        writeln!(f, "[2026-10-12T10:42:09+0200] [ALPM] transaction completed").unwrap();
        writeln!(f, "[2026-10-12T10:43:00+0200] [PACMAN] Running 'pacman -Q'").unwrap();
        drop(f);
        let t = last_completed_transaction(&p).unwrap();
        assert_eq!(
            t,
            DateTime::parse_from_rfc3339("2026-10-12T10:42:09+02:00").unwrap()
        );
        assert!(last_completed_transaction(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn lock_detection() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("arch-update.lock");
        assert!(!lock_is_held(&p));
        let holder = File::create(&p).unwrap();
        assert!(!lock_is_held(&p));
        // SAFETY: Testdatei
        unsafe {
            assert_eq!(
                libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB),
                0
            );
        }
        assert!(lock_is_held(&p));
        drop(holder);
        assert!(!lock_is_held(&p));
    }
}
