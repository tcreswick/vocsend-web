//! Runs the web engine as an ordinary process, with `aplay` and `arecord`
//! as its speaker and microphone, so it can be tested against the
//! command-line vocsend in the simulator (see vocsend's tests/sim).
//!
//!   bridge rx [replies] [KEY]         writes what it receives to stdout
//!   bridge tx FILE LEVEL [negotiate] [KEY]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use vocsend_web::engine::{Engine, Event};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |w: &str| args.iter().any(|a| a == w);
    let key = args.iter().find_map(|a| a.strip_prefix("key="));
    let mut engine = if args[0] == "rx" {
        Engine::receiver(has("replies"), key, None).unwrap()
    } else {
        let data = std::fs::read(&args[1]).unwrap();
        Engine::sender(&data, Some(&args[1]), args[2].parse().unwrap(), has("negotiate"), key, None).unwrap()
    };
    let raw = ["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"];
    let mut mic = Command::new("arecord").args(raw).stdout(Stdio::piped()).spawn().unwrap();
    let mut speaker = Command::new("aplay").args(raw).stdin(Stdio::piped()).spawn().unwrap();
    let mut heard = mic.stdout.take().unwrap();
    let (to_speaker, queue) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut out = speaker.stdin.take().unwrap();
    std::thread::spawn(move || {
        for bytes in queue {
            let _ = out.write_all(&bytes).and_then(|_| out.flush());
        }
    });
    let started = std::time::Instant::now();
    let ok;
    'run: loop {
        let mut bytes = [0u8; 3200];
        heard.read_exact(&mut bytes).unwrap();
        let samples: Vec<f32> = bytes.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
        engine.feed(&samples, started.elapsed().as_secs_f64() * 1000.0);
        while let Some(audio) = engine.take_audio() {
            let pcm: Vec<u8> = audio.iter().flat_map(|&v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
            to_speaker.send(pcm).unwrap();
        }
        for event in engine.take_events() {
            match event {
                Event::Received(r) => {
                    eprintln!("bridge: received {} bytes, level {}, {} repaired, fingerprint {}", r.data.len(), r.level, r.repaired, r.fingerprint);
                    std::io::stdout().write_all(&r.data).unwrap();
                }
                Event::Done { ok: good, text } => {
                    eprintln!("bridge: done ({good}): {text}");
                    ok = good;
                    break 'run;
                }
                Event::Progress { .. } | Event::Speaking { .. } => {}
                other => eprintln!("bridge: {other:?}"),
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = mic.kill();
    std::process::exit(if ok { 0 } else { 1 });
}
