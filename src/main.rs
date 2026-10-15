//! Entry point: parse the command line, start the phone and the web interface.

use std::path::PathBuf;
use std::sync::Arc;

use piphone::lockfile::{self, InstanceLock};
use piphone::phone::Options;
use piphone::webserver;
use piphone::Phone;

/// The version is taken from `Cargo.toml` through the environment, so the help
/// cannot fall behind the package version.
const HELP: &str = concat!(
    "\
Steuerung für ein Raspberry-Pi-basiertes Headless-Telefon im Gehäuse alter Telefone (bspw. FeTAp).
Koordiniert Hörer, Lautsprecher, Gabelkontakt, Nummernschalter und SIP-Client (rvoip).

Version: ",
    env!("CARGO_PKG_VERSION"),
    "

Aufruf:
  piphone [Optionen]

Optionen:
  -c, --config <DATEI>   Pfad zur Konfigurationsdatei (Standard: /usr/local/etc/piphone/config.toml)
      --sounds <ORDNER>  Verzeichnis der Klangdateien (Standard: /usr/local/share/piphone/sounds)
      --ignore-dnd       Nicht stören für Testzwecke deaktivieren
      --no-web           Webserver zur Steuerung nicht starten
      --silent-boot      Keinen Start-Sound abspielen
  -v, --verbose           Ausführliches Logging aktivieren
  -h, --help              Diese Hilfe anzeigen
"
);

fn main() {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return,
        Err(message) => {
            eprintln!("{}", message);
            std::process::exit(2);
        }
    };

    // Only one instance may use the dial, the LEDs and the sound card. 
    // `lock` lives until the end of `main`, which is what
    // holds the claim for as long as the phone runs.
    let lock_path = lockfile::default_path();
    let lock = match InstanceLock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(message) => {
            eprintln!("{}", message);
            std::process::exit(1);
        }
    };
    println!("Sperrdatei: {}", lock.path().display());

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Laufzeitumgebung konnte nicht gestartet werden: {}", e);
            std::process::exit(1);
        }
    };

    if let Err(message) = runtime.block_on(run(options)) {
        eprintln!("{}", message);
        std::process::exit(1);
    }

    drop(lock);
}

/// Parse `argv`, returning `None` when help was printed and the program should stop.
fn parse_args() -> Result<Option<Options>, String> {
    parse_args_from(std::env::args().skip(1))
}

/// The body of [`parse_args`], taking the arguments so that this can be tested.
fn parse_args_from<I: Iterator<Item = String>>(mut args: I) -> Result<Option<Options>, String> {
    let mut options = Options::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                let value = args.next().ok_or("Option --config braucht einen Pfad.")?;
                options.config = PathBuf::from(value);
            }
            "--sounds" => {
                let value = args.next().ok_or("Option --sounds braucht einen Pfad.")?;
                options.sounds_dir = PathBuf::from(value);
            }
            "--ignore-dnd" => options.ignore_dnd = true,
            "--no-web" => options.no_web = true,
            "--silent-boot" => options.silent_boot = true,
            "-v" | "--verbose" => options.verbose = true,
            "-h" | "--help" => {
                println!("{}", HELP);
                return Ok(None);
            }
            other => return Err(format!("Unbekannte Option: {}", other)),
        }
    }

    Ok(Some(options))
}

async fn run(options: Options) -> Result<(), String> {
    println!("Starte PiPhone...");

    let phone = Arc::new(Phone::new(options.clone()).await?);

    // Watchdog keeps the SIP endpoint in step with the network connection.
    let watchdog_phone = phone.clone();
    tokio::spawn(async move { watchdog_phone.watchdog().await });

    // Web interface, unless disabled.
    if !options.no_web {
        let port = piphone::config::option_int(phone.config(), "web", "port");
        if port > 0 {
            let user = piphone::config::option_str(phone.config(), "web", "user");
            let password = piphone::config::option_str(phone.config(), "web", "pass");
            let credentials = if !user.is_empty() && !password.is_empty() {
                Some(format!("{}:{}", user, password))
            } else {
                None
            };

            let app = webserver::router(phone.clone(), credentials, options.verbose);
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port as u16))
                .await
                .map_err(|e| format!("Webserver nicht startbar: {}", e))?;

            println!("Webserver läuft auf Port {}.", port);
            tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, app).await {
                    eprintln!("Webserver beendet: {}", e);
                }
            });
        }
    }

    println!("Registrierte Zielrufnummern:");
    for (number, target) in phone.numbers() {
        println!(" - {} -> {}", number, target);
    }
    println!("Bereit.");

    // Wait for SIGTERM/SIGINT, then shut down cleanly.
    wait_for_termination().await;

    // Let the SIP stack say goodbye before the process goes: a call that is up
    // should end with a BYE and a cancelled registration rather than with the
    // RTP simply stopping.
    phone.stop_sip().await;

    println!("PiPhone beendet.");
    Ok(())
}

/// Resolve when the process is asked to stop.
async fn wait_for_termination() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("SIGTERM nicht abfangbar: {}", e);
            std::future::pending::<()>().await;
            return;
        }
    };

    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(sig) => sig,
        Err(e) => {
            eprintln!("SIGINT nicht abfangbar: {}", e);
            std::future::pending::<()>().await;
            return;
        }
    };

    tokio::select! {
        _ = term.recv() => println!("\nSIGTERM empfangen, beende."),
        _ = interrupt.recv() => println!("\nSIGINT empfangen, beende."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(list: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
        list.iter().map(|a| a.to_string())
    }

    /// The help has to work while the phone is running, so asking for it must
    /// not need the lock: `main` returns on `Ok(None)` before it takes it.
    #[test]
    fn asking_for_the_help_stops_before_anything_else() {
        assert!(matches!(parse_args_from(args(&["--help"])), Ok(None)));
        assert!(matches!(parse_args_from(args(&["-h"])), Ok(None)));
        // Even with everything else asked for as well, and even behind the
        // running instance's lock.
        assert!(matches!(
            parse_args_from(args(&["--no-web", "--help"])),
            Ok(None)
        ));
    }

    /// A run that is not the help goes on to take the lock.
    #[test]
    fn a_run_without_the_help_is_a_run() {
        let options = parse_args_from(args(&["--no-web"]))
            .expect("gültige Optionen")
            .expect("kein Hilfeabruf");
        assert!(options.no_web);
    }
}
