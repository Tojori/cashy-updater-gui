//! Zähler-Badge am Taskleisten-Symbol über `com.canonical.Unity.LauncherEntry`.
//!
//! Die Schnittstelle besteht aus einem einzigen Signal `Update(s app_uri, a{sv} properties)`,
//! das Taskleisten wie der Plasma-Task-Manager, Dash to Dock/Dash to Panel, Plank oder
//! Latte auswerten. `app_uri` ist `application://<desktop-datei>`; die Taskleiste
//! ordnet das Fenster über dessen `app_id` bzw. `StartupWMClass` dieser Desktop-Datei zu.
//!
//! Der Versand läuft in einem eigenen Thread, damit die Oberfläche nie auf den
//! Session-Bus wartet. Fehlt der Bus oder unterstützt die Taskleiste keine Badges,
//! passiert nichts weiter als ein Debug-Eintrag im Protokoll.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use zbus::blocking::Connection;
use zbus::zvariant::Value;

/// Name der Desktop-Datei (ohne `.desktop`), gleichzeitig Wayland-`app_id`.
pub const APP_ID: &str = "cashy-updater-gui";

const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
const SIGNAL: &str = "Update";

pub fn app_uri(app_id: &str) -> String {
    format!("application://{app_id}.desktop")
}

/// Objektpfad des Signals. Der Pfad ist frei wählbar, muss aber gültig sein;
/// libunity leitet ihn aus einem Hash der URI ab, hier genügt die `app_id`.
fn object_path(app_id: &str) -> String {
    let id: String = app_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("/com/canonical/unity/launcherentry/{id}")
}

/// Eigenschaften für `Update`: Zähler sichtbar ab 1, sonst ausgeblendet.
pub fn properties(count: u32) -> HashMap<&'static str, Value<'static>> {
    HashMap::from([
        ("count", Value::from(i64::from(count))),
        ("count-visible", Value::from(count > 0)),
    ])
}

enum Msg {
    Set(u32),
    Quit,
}

/// Wie die Verbindung zum Session-Bus hergestellt wird (Tests: privater Bus).
pub type Connect = Box<dyn FnOnce() -> zbus::Result<Connection> + Send>;

pub struct LauncherBadge {
    tx: Sender<Msg>,
    last: Option<u32>,
    worker: Option<JoinHandle<()>>,
}

impl LauncherBadge {
    /// Badge für die installierte Desktop-Datei über den Session-Bus.
    pub fn session() -> Self {
        Self::with_connection(APP_ID, Box::new(Connection::session))
    }

    /// Ohne Wirkung (Tests, die nicht auf den Session-Bus des Nutzers schreiben dürfen).
    pub fn disabled() -> Self {
        let (tx, _) = mpsc::channel();
        Self {
            tx,
            last: None,
            worker: None,
        }
    }

    pub fn with_connection(app_id: &str, connect: Connect) -> Self {
        let (tx, rx) = mpsc::channel();
        let app_id = app_id.to_string();
        let worker = thread::Builder::new()
            .name("launcher-badge".into())
            .spawn(move || worker(&app_id, connect, rx))
            .map_err(|e| tracing::debug!("Badge-Thread nicht gestartet: {e}"))
            .ok();
        Self {
            tx,
            last: None,
            worker,
        }
    }

    /// Setzt den Zähler; unveränderte Werte werden nicht erneut gesendet.
    pub fn set(&mut self, count: u32) {
        if self.last == Some(count) {
            return;
        }
        self.last = Some(count);
        let _ = self.tx.send(Msg::Set(count));
    }

    /// Blendet den Zähler aus und wartet, bis das Signal versendet ist.
    pub fn shutdown(&mut self) {
        if self.last.is_some_and(|c| c > 0) {
            let _ = self.tx.send(Msg::Set(0));
        }
        self.last = None;
        let _ = self.tx.send(Msg::Quit);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LauncherBadge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(app_id: &str, connect: Connect, rx: Receiver<Msg>) {
    // Erst verbinden, wenn wirklich etwas zu senden ist.
    let first = match rx.recv() {
        Ok(Msg::Set(c)) => c,
        Ok(Msg::Quit) | Err(_) => return,
    };
    let conn = match connect() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("Kein Session-Bus für das Taskleisten-Badge: {e}");
            // Restliche Nachrichten verwerfen, bis die App endet.
            while let Ok(Msg::Set(_)) = rx.recv() {}
            return;
        }
    };
    let uri = app_uri(app_id);
    let path = object_path(app_id);
    let mut warned = false;
    let mut send = |count: u32| {
        let body = (uri.as_str(), properties(count));
        match conn.emit_signal(None::<&str>, path.as_str(), INTERFACE, SIGNAL, &body) {
            Ok(()) => tracing::debug!("Taskleisten-Badge: {count}"),
            Err(e) if !warned => {
                warned = true;
                tracing::debug!("Taskleisten-Badge konnte nicht gesetzt werden: {e}");
            }
            Err(_) => {}
        }
    };
    send(first);
    while let Ok(Msg::Set(c)) = rx.recv() {
        send(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_and_path_are_valid() {
        assert_eq!(app_uri(APP_ID), "application://cashy-updater-gui.desktop");
        let p = object_path(APP_ID);
        assert_eq!(p, "/com/canonical/unity/launcherentry/cashy_updater_gui");
        assert!(zbus::zvariant::ObjectPath::try_from(p.as_str()).is_ok());
    }

    #[test]
    fn count_zero_hides_badge() {
        let p = properties(0);
        assert_eq!(p["count-visible"], Value::from(false));
        let p = properties(7);
        assert_eq!(p["count"], Value::from(7i64));
        assert_eq!(p["count-visible"], Value::from(true));
    }

    #[test]
    fn missing_bus_is_harmless() {
        let mut b = LauncherBadge::with_connection(
            APP_ID,
            Box::new(|| Err(zbus::Error::Failure("kein Bus".into()))),
        );
        b.set(3);
        b.set(3);
        b.set(0);
        b.shutdown();
    }
}
