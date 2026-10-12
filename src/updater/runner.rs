//! Ausführung externer Programme ohne Shell.
//!
//! Alle Aufrufe verwenden feste, strukturierte Argumentlisten – es wird nie
//! eine Zeichenkette an eine Shell übergeben. Laufende Prozesse können über ein
//! [`CancelToken`] beendet werden (SIGTERM an die eigene Prozessgruppe).

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Kooperatives Abbruchsignal für Hintergrundaufgaben.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    /// Exit-Code; `None` bei Beendigung durch ein Signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub cancelled: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.cancelled
    }
}

/// Abstraktion für Prozessaufrufe (Test-Doubles in Tests).
pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        env: &[(String, String)],
        stdin_null: bool,
        cancel: Option<&CancelToken>,
    ) -> io::Result<CommandOutput>;
}

/// Echte Prozessausführung.
#[derive(Debug, Clone, Default)]
pub struct SystemRunner {
    /// Optionales Zeitlimit (Schutz gegen hängende Prozesse).
    pub timeout: Option<Duration>,
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        program: &Path,
        args: &[&str],
        env: &[(String, String)],
        stdin_null: bool,
        cancel: Option<&CancelToken>,
    ) -> io::Result<CommandOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Eigene Prozessgruppe, damit ein Abbruch auch Kindprozesse erreicht.
            .process_group(0);
        if stdin_null {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn()?;
        let out = spawn_reader(child.stdout.take().expect("stdout piped"));
        let err = spawn_reader(child.stderr.take().expect("stderr piped"));
        let pid = child.id() as libc::pid_t;
        let started = Instant::now();
        let mut cancelled = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let timed_out = self.timeout.is_some_and(|t| started.elapsed() > t);
            if !cancelled && (cancel.is_some_and(CancelToken::is_cancelled) || timed_out) {
                cancelled = true;
                // SAFETY: Signal an die eigene, gerade erzeugte Prozessgruppe.
                unsafe {
                    libc::kill(-pid, libc::SIGTERM);
                }
            }
            thread::sleep(Duration::from_millis(50));
        };
        Ok(CommandOutput {
            code: status.code(),
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
            cancelled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_without_shell_interpolation() {
        let r = SystemRunner::default();
        let out = r
            .run(
                Path::new("/bin/echo"),
                &["$HOME; rm -rf /"],
                &[],
                true,
                None,
            )
            .unwrap();
        assert_eq!(out.stdout, "$HOME; rm -rf /\n");
        assert!(out.success());
    }

    #[test]
    fn cancel_terminates_process() {
        let r = SystemRunner::default();
        let token = CancelToken::new();
        let t2 = token.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            t2.cancel();
        });
        let started = Instant::now();
        let out = r
            .run(Path::new("/bin/sleep"), &["10"], &[], true, Some(&token))
            .unwrap();
        h.join().unwrap();
        assert!(out.cancelled);
        assert!(!out.success());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
