//! Einstiegspunkt von `cashy-updater-gui`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    if let Some(arg) = args.next() {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("cashy-updater-gui {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!(
                    "cashy-updater-gui {}\n\
                     Grafische Oberfläche für cachy-update (arch-update).\n\n\
                     Aufruf: cashy-updater-gui [--version | --help]",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("cashy-updater-gui: unbekannte Option „{other}“ (siehe --help)");
                return ExitCode::from(2);
            }
        }
    }
    cashy_updater_gui::app::run()
}
