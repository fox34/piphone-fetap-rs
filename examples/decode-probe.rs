//! Diagnostic: decode every file in `sounds/` through the same path the
//! player uses, without touching a sound card.
//!
//!     cargo run --example decode-probe -- sounds

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "sounds".to_string());
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {}", dir, e))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav" || x == "mp3"))
        .collect();
    entries.sort();

    for path in entries {
        match probe(&path) {
            Ok(report) => println!("{:<28} {}", path.display(), report),
            Err(e) => println!("{:<28} FEHLER {}", path.display(), e),
        }
    }
}

fn probe(path: &Path) -> Result<String, String> {
    let text = path.to_string_lossy().to_string();

    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("nicht lesbar: {}", e))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "keine Tonspur".to_string())?;

    let params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => return Err("keine Tonspur".to_string()),
    };

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| format!("kein Decoder: {}", e))?;

    let mut packets = 0usize;
    let mut samples = 0usize;
    let mut skip_errors = 0usize;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(e) => return Err(format!("Packet-Fehler: {}", e)),
        };
        packets += 1;

        match decoder.decode(&packet) {
            Ok(decoded) => samples += decoded.samples_interleaved(),
            Err(_) => skip_errors += 1,
        }
    }

    let channels = params.channels.as_ref().map(|c| c.count()).unwrap_or(0);
    let secs = match params.sample_rate {
        Some(rate) if channels > 0 => samples as f64 / (rate as f64 * channels as f64),
        _ => 0.0,
    };

    Ok(format!(
        "{:>4} Hz  {:>2} Kanal  {:>8} Hz  {:>3} Pakete  {:>5} Fehler  {:>6.2} s",
        params.sample_rate.unwrap_or(0),
        channels,
        params.sample_rate.unwrap_or(0),
        packets,
        skip_errors,
        secs,
    ))
    .map(|s| format!("{}  {}", text, s))
}