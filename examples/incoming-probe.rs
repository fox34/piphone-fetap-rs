//! Minimal incoming-call test, to tell rvoip apart from the phone's own logic.
//!
//! Registers with the same builder settings the phone uses, waits for one
//! incoming call, answers it immediately, and reports what happens. It shares
//! nothing with the phone except the crate, so a failure here points at rvoip
//! or the environment rather than at our code.
//!
//! The advertised port must match the bound port, otherwise the registrar
//! registers a contact it cannot reach and registration never completes.
//!
//!     SIP_REGISTRAR=sip:10.0.0.1 SIP_USERNAME=… SIP_PASSWORD=… \
//!     SIP_BIND=0.0.0.0:5070 SIP_ADVERTISED_ADDR=10.0.0.15:5070 \
//!     cargo run --release --example incoming-probe

use std::time::Duration;

use rvoip::sip::{Endpoint, EndpointEvent, EndpointProfile};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{} fehlt", name))
}

#[tokio::main]
async fn main() -> rvoip::sip::Result<()> {
    let registrar = env("SIP_REGISTRAR");
    let username = env("SIP_USERNAME");
    let password = env("SIP_PASSWORD");
    let bind: std::net::SocketAddr = env("SIP_BIND").parse().expect("SIP_BIND unbrauchbar");
    let advertised: std::net::SocketAddr =
        env("SIP_ADVERTISED_ADDR").parse().expect("SIP_ADVERTISED_ADDR unbrauchbar");

    let endpoint = match Endpoint::builder()
        .name(&username)
        .account(&username)
        .password(password)
        .registrar(registrar)
        .profile(EndpointProfile::LanPbx)
        .bind_addr(bind)
        .advertised_addr(advertised)
        .media_ports(31000, 31100)
        .build()
        .await
    {
        Ok(endpoint) => endpoint,
        Err(e) => {
            println!("Probe: FEHLER beim Aufbau des Endpunkts: {}", e);
            return Ok(());
        }
    };

    // Split rather than `events()`: the observational stream carries no session
// lifecycle handle, so an incoming call taken from it cannot be answered.
let (endpoint, mut events) = endpoint.split();

    if let Err(e) = endpoint.register().await {
        println!("Probe: FEHLER bei der Registrierung: {}", e);
        return Ok(());
    }

    println!("Probe: registriert auf {}, warte auf Anruf …", advertised);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);

    loop {
        if tokio::time::Instant::now() > deadline {
            println!("Probe: kein Anruf innerhalb von 180s.");
            return Ok(());
        }

        let event = tokio::time::timeout(Duration::from_secs(5), events.next()).await;

        let Ok(Ok(Some(event))) = event else {
            continue; // timeout, stream error or stream closed
        };

        match event {
            EndpointEvent::IncomingCall(incoming) => {
                println!("Probe: Anruf von {:?}, nehme sofort an …", incoming.from());

                // Answer straight away: no waiting on a human, which is the one
                // thing the phone does that this probe deliberately does not.
                match incoming.answer().await {
                    Ok(call) => {
                        println!("Probe: angenommen, id {:?}", call.id());
                        match call.audio().await {
                            Ok(audio) => {
                                println!("Probe: Audio verfügbar");
                                drop(audio);
                            }
                            Err(e) => println!("Probe: FEHLER bei audio(): {}", e),
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        println!("Probe: lege auf …");
                        if let Err(e) = call.hangup().await {
                            println!("Probe: FEHLER beim Auflegen: {}", e);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        println!("Probe: FEHLER beim Annehmen: {}", e);
                        return Ok(());
                    }
                }
            }
            _ => println!("Probe: weiteres Ereignis"),
        }
    }
}