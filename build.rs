//! Puts the build date into the environment, so `--help` can show it.
//!
//! Deliberately no `cargo:rerun-if-changed` here: a build script that emits one
//! of those is rerun only for that path, which would freeze the date at whatever
//! it was the first time. Without it, cargo reruns this whenever the package is
//! rebuilt, which is what keeps the date honest.

fn main() {
    // Build time
    let date = std::process::Command::new("date")
        .arg("-u")
        .arg("+%Y-%m-%dT%H:%M:%SZ")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|date| date.trim().to_string())
        .filter(|date| !date.is_empty())
        // Never fail the build over a missing `date`; an unknown date beats no
        // binary at all.
        .unwrap_or_else(|| "unbekannt".to_string());
    
    // rustc version
    let rustc_version = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "rustc unbekannt".to_string());
    
    println!("cargo:rustc-env=DATE={} ({})", date, rustc_version);
}