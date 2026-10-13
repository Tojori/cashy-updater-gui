//! Taskleisten-Badge gegen einen privaten `dbus-daemon` (nicht den Session-Bus des Nutzers).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use cashy_updater_gui::launcher_badge::{APP_ID, LauncherBadge, app_uri};
use zbus::blocking::{Connection, MessageIterator, connection};
use zbus::zvariant::OwnedValue;

struct PrivateBus {
    child: Child,
    address: String,
    _dir: tempfile::TempDir,
}

impl PrivateBus {
    /// `None`, wenn kein `dbus-daemon` installiert ist (Test wird dann übersprungen).
    fn start() -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!(
                "--address=unix:path={}",
                dir.path().join("bus").display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Some(Self {
            child,
            address: line.trim().to_string(),
            _dir: dir,
        })
    }

    fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .unwrap()
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Update = (String, HashMap<String, OwnedValue>);

#[test]
fn badge_emits_unity_launcher_entry_updates() {
    let Some(bus) = PrivateBus::start() else {
        eprintln!("dbus-daemon nicht verfügbar – Test übersprungen");
        return;
    };
    let listener = bus.connect();
    let iter = MessageIterator::for_match_rule(
        "type='signal',interface='com.canonical.Unity.LauncherEntry',member='Update'",
        &listener,
        None,
    )
    .unwrap();
    let (tx, rx) = mpsc::channel::<Update>();
    std::thread::spawn(move || {
        for msg in iter {
            let Ok(msg) = msg else { break };
            if tx
                .send(msg.body().deserialize::<Update>().unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let recv = || {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("kein Signal")
    };

    let address = bus.address.clone();
    let mut badge = LauncherBadge::with_connection(
        APP_ID,
        Box::new(move || connection::Builder::address(address.as_str())?.build()),
    );

    badge.set(5);
    let (uri, props) = recv();
    assert_eq!(uri, app_uri(APP_ID));
    assert_eq!(i64::try_from(&props["count"]).unwrap(), 5);
    assert!(bool::try_from(&props["count-visible"]).unwrap());

    badge.set(5); // unverändert → kein zweites Signal
    badge.set(2);
    let (_, props) = recv();
    assert_eq!(i64::try_from(&props["count"]).unwrap(), 2);

    // Beim Beenden wird der Zähler ausgeblendet.
    badge.shutdown();
    let (_, props) = recv();
    assert!(!bool::try_from(&props["count-visible"]).unwrap());
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
}
