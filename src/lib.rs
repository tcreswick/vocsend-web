//! vocsend for the browser: the engine, and the thin layer that lets a web
//! page drive it.

pub mod engine;

use engine::{Engine, Event};
use wasm_bindgen::prelude::*;

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One event as a JSON object, and the data it carries if it is a transfer.
fn json(event: Event) -> (String, Option<Vec<u8>>) {
    match event {
        Event::Log(text) => (format!(r#"{{"type":"log","text":{}}}"#, quote(&text)), None),
        Event::Status(text) => (format!(r#"{{"type":"status","text":{}}}"#, quote(&text)), None),
        Event::Speaking { seconds, what } => (format!(r#"{{"type":"speaking","seconds":{seconds},"what":{}}}"#, quote(&what)), None),
        Event::Progress { label, fraction } => (format!(r#"{{"type":"progress","label":{},"fraction":{fraction}}}"#, quote(&label)), None),
        Event::Agreed { level, redundancy, bits_per_second } => (
            format!(r#"{{"type":"agreed","level":{level},"redundancy":{redundancy},"bitsPerSecond":{bits_per_second}}}"#),
            None,
        ),
        Event::Burst { level, wrong, of, loudness } => (
            format!(r#"{{"type":"burst","level":{level},"wrong":{wrong},"of":{of},"loudness":{loudness},"verdict":{}}}"#,
                quote(vocsend::plan::verdict(level, wrong as f64 / of as f64))),
            None,
        ),
        Event::Problem(text) => (format!(r#"{{"type":"problem","text":{}}}"#, quote(&text)), None),
        Event::Done { ok, text } => (format!(r#"{{"type":"done","ok":{ok},"text":{}}}"#, quote(&text)), None),
        Event::Received(r) => (
            format!(
                r#"{{"type":"received","name":{},"bytes":{},"level":{},"protocol":{},"compressed":{},"carried":{},"encrypted":{},"addressed":{},"negotiated":{},"blocks":{},"repaired":{},"retries":{},"fingerprint":{},"seconds":{}}}"#,
                r.name.as_deref().map(quote).unwrap_or_else(|| "null".into()),
                r.data.len(), r.level, r.protocol, r.compressed, r.carried, r.encrypted, r.addressed,
                r.negotiated, r.blocks, r.repaired, r.retries, quote(&r.fingerprint), r.seconds
            ),
            Some(r.data),
        ),
    }
}

/// One sending, receiving or testing session.
#[wasm_bindgen]
pub struct Session {
    engine: Engine,
    data: std::collections::VecDeque<Vec<u8>>,
}

fn optional(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.is_empty())
}

#[wasm_bindgen]
impl Session {
    /// Listen for a transfer or for test bursts. With `allow_replies` it
    /// answers a sender that negotiates; without, it never makes a sound.
    pub fn receiver(allow_replies: bool, key: Option<String>, address: Option<String>) -> Result<Session, JsError> {
        let engine = Engine::receiver(allow_replies, optional(key).as_deref(), optional(address).as_deref()).map_err(|e| JsError::new(&e))?;
        Ok(Session { engine, data: Default::default() })
    }

    /// Send `data`, one-way at `level` or negotiated.
    pub fn sender(data: &[u8], name: Option<String>, level: u32, negotiate: bool, key: Option<String>, address: Option<String>) -> Result<Session, JsError> {
        let engine = Engine::sender(data, optional(name).as_deref(), level as usize, negotiate, optional(key).as_deref(), optional(address).as_deref())
            .map_err(|e| JsError::new(&e))?;
        Ok(Session { engine, data: Default::default() })
    }

    /// Play test bursts at each level from `lo` to `hi`, round and round.
    pub fn tester(lo: u32, hi: u32) -> Session {
        Session { engine: Engine::tester(lo as usize, hi as usize), data: Default::default() }
    }

    /// Microphone samples at 16 kHz, and the time now in milliseconds.
    pub fn feed(&mut self, samples: &[f32], now_ms: f64) {
        self.engine.feed(samples, now_ms);
    }

    /// Let time pass without new audio.
    pub fn tick(&mut self, now_ms: f64) {
        self.engine.tick(now_ms);
    }

    /// The next piece of 16 kHz sound to play, if any. Play it at once.
    pub fn take_audio(&mut self) -> Option<Vec<f32>> {
        self.engine.take_audio()
    }

    /// What has happened since the last call, as a JSON array.
    pub fn take_events(&mut self) -> String {
        let mut out = Vec::new();
        for event in self.engine.take_events() {
            let (text, data) = json(event);
            if let Some(data) = data {
                self.data.push_back(data);
            }
            out.push(text);
        }
        format!("[{}]", out.join(","))
    }

    /// The bytes of the oldest "received" event not yet collected.
    pub fn take_data(&mut self) -> Option<Vec<u8>> {
        self.data.pop_front()
    }

    pub fn finished(&self) -> bool {
        self.engine.finished()
    }
}

/// Converts sound between a device's sample rate and vocsend's 16 kHz.
#[wasm_bindgen]
pub struct Resampler(vocsend::resample::Resampler);

#[wasm_bindgen]
impl Resampler {
    #[wasm_bindgen(constructor)]
    pub fn new(from: u32, to: u32) -> Resampler {
        Resampler(vocsend::resample::Resampler::new(from, to))
    }

    pub fn push(&mut self, input: &[f32]) -> Vec<f32> {
        self.0.push(input)
    }
}

/// The speed levels, as a JSON array for the page to show.
#[wasm_bindgen]
pub fn levels() -> String {
    let rows: Vec<String> = (1..=10)
        .map(|n| {
            let l = vocsend::modem::level(n);
            format!(
                r#"{{"level":{n},"pitches":{},"ms":{},"redundancy":{},"bitsPerSecond":{}}}"#,
                l.pitches, l.ms, l.redundancy, vocsend::modem::speed(n, l.redundancy)
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// Roughly how long sending `bytes` will take, in seconds.
#[wasm_bindgen]
pub fn estimate(bytes: u32, level: u32, negotiate: bool, encrypted: bool) -> f64 {
    let lv = (level as usize).clamp(1, 10);
    let r = vocsend::modem::level(lv).redundancy;
    vocsend::plan::estimate(bytes as usize + 1, lv, r, negotiate, encrypted).1
}

/// A short fingerprint of some data, the same one the command line prints.
#[wasm_bindgen]
pub fn fingerprint(data: &[u8]) -> String {
    vocsend::crypto::fingerprint(data)
}

#[wasm_bindgen]
pub fn protocol_version() -> u32 {
    vocsend::modem::PROTOCOL as u32
}
