//! Two engines talking through simulated air, on a clock that only moves
//! when the test moves it.

use vocsend::dsp::Rng;
use vocsend_web::engine::{Engine, Event, Received};

const STEP: usize = 1600; // 100 ms

struct Side {
    engine: Engine,
    /// Everything this side has played, by sample time.
    tape: Vec<f32>,
    events: Vec<Event>,
}

impl Side {
    fn new(engine: Engine) -> Side {
        Side { engine, tape: Vec::new(), events: Vec::new() }
    }

    fn collect(&mut self, at: usize) {
        while let Some(audio) = self.engine.take_audio() {
            let start = self.tape.len().max(at);
            self.tape.resize(start, 0.0);
            self.tape.extend(audio);
        }
        self.events.extend(self.engine.take_events());
    }

    fn slice(&self, at: usize) -> Vec<f32> {
        (at..at + STEP).map(|i| self.tape.get(i).copied().unwrap_or(0.0)).collect()
    }

    fn received(&self) -> Vec<&Received> {
        self.events.iter().filter_map(|e| if let Event::Received(r) = e { Some(&**r) } else { None }).collect()
    }

    fn done(&self) -> Option<(bool, &str)> {
        self.events.iter().find_map(|e| if let Event::Done { ok, text } = e { Some((*ok, text.as_str())) } else { None })
    }

    fn problems(&self) -> Vec<&str> {
        self.events.iter().filter_map(|e| if let Event::Problem(t) = e { Some(t.as_str()) } else { None }).collect()
    }
}

/// Run both sides until `until` says stop or the time runs out. Each hears
/// the other at half volume, itself more quietly, and some noise.
fn run(a: &mut Side, b: &mut Side, seconds: usize, noise: f64, until: impl Fn(&Side, &Side) -> bool) {
    let mut rng = Rng::new(42);
    for step in 0..seconds * 10 {
        let at = step * STEP;
        let now = (step * 100) as f64;
        let (from_a, from_b) = (a.slice(at), b.slice(at));
        let mut hear = |other: &[f32], own: &[f32]| -> Vec<f32> {
            (0..STEP).map(|i| 0.5 * other[i] + 0.3 * own[i] + (noise * rng.gaussian()) as f32).collect()
        };
        let (to_a, to_b) = (hear(&from_b, &from_a), hear(&from_a, &from_b));
        a.engine.feed(&to_a, now);
        a.collect(at + STEP);
        b.engine.feed(&to_b, now);
        b.collect(at + STEP);
        if until(a, b) {
            return;
        }
    }
}

fn payload(n: usize) -> Vec<u8> {
    let mut rng = Rng::new(n as u64);
    (0..n).map(|_| rng.below(256) as u8).collect()
}

#[test]
fn one_way_transfer() {
    let data = payload(80);
    let mut tx = Side::new(Engine::sender(&data, Some("key.pub"), 8, false, None, None).unwrap());
    let mut rx = Side::new(Engine::receiver(false, None, None).unwrap());
    run(&mut tx, &mut rx, 60, 0.003, |_, rx| rx.done().is_some());
    let got = rx.received();
    assert_eq!(got.len(), 1, "{:?}", rx.events);
    assert_eq!(got[0].data, data);
    assert_eq!(got[0].name.as_deref(), Some("key.pub"));
    assert_eq!((got[0].level, got[0].negotiated, got[0].encrypted), (8, false, false));
    assert!(rx.tape.iter().all(|&v| v == 0.0), "a one-way receiver must never make a sound");
    assert_eq!(rx.done().map(|d| d.0), Some(true));
}

#[test]
fn negotiated_transfer_picks_a_level_and_acknowledges() {
    let data = payload(500);
    let mut tx = Side::new(Engine::sender(&data, None, 4, true, None, None).unwrap());
    let mut rx = Side::new(Engine::receiver(true, None, None).unwrap());
    run(&mut tx, &mut rx, 240, 0.003, |tx, rx| tx.done().is_some() && rx.done().is_some());
    assert_eq!(tx.done().map(|d| d.0), Some(true), "{:?}", tx.events);
    let got = rx.received();
    assert_eq!(got.len(), 1, "{:?}", rx.events);
    assert_eq!(got[0].data, data);
    assert!(got[0].negotiated && got[0].blocks >= 2 && got[0].level >= 6, "{:?}", got[0]);
    assert!(tx.events.iter().any(|e| matches!(e, Event::Agreed { .. })));
}

#[test]
fn a_noisy_link_settles_lower_and_still_delivers() {
    let data = payload(100);
    let mut tx = Side::new(Engine::sender(&data, None, 4, true, None, None).unwrap());
    let mut rx = Side::new(Engine::receiver(true, None, None).unwrap());
    run(&mut tx, &mut rx, 300, 0.07, |tx, rx| tx.done().is_some() && rx.done().is_some());
    assert_eq!(tx.done().map(|d| d.0), Some(true), "{:?}", tx.events);
    assert_eq!(rx.received()[0].data, data);
    assert!(rx.received()[0].level < 10);
}

#[test]
fn encryption_and_addresses() {
    let data = b"the launch codes".to_vec();
    let mut tx = Side::new(Engine::sender(&data, None, 8, false, Some("hunter2"), Some("laptop")).unwrap());
    let mut rx = Side::new(Engine::receiver(false, Some("hunter2"), Some("laptop")).unwrap());
    run(&mut tx, &mut rx, 60, 0.003, |_, rx| rx.done().is_some());
    assert_eq!(rx.received()[0].data, data);
    assert!(rx.received()[0].encrypted && rx.received()[0].addressed);

    // The wrong key, the wrong address, and no key at all.
    for (key, address, expect) in [(Some("wrong"), Some("laptop"), "decrypt"), (None, None, "ignoring"), (Some("hunter2"), Some("desktop"), "ignoring")] {
        let mut tx = Side::new(Engine::sender(&data, None, 8, false, Some("hunter2"), Some("laptop")).unwrap());
        let mut rx = Side::new(Engine::receiver(false, key, address).unwrap());
        run(&mut tx, &mut rx, 40, 0.003, |tx, _| tx.done().is_some());
        assert!(rx.received().is_empty());
        let said: String = rx.events.iter().map(|e| format!("{e:?}")).collect();
        assert!(said.contains(expect), "{key:?} {address:?}: {said}");
    }
    assert!(Engine::sender(&data, None, 8, false, None, Some("laptop")).is_err());
}

#[test]
fn a_one_way_receiver_refuses_to_negotiate() {
    let mut tx = Side::new(Engine::sender(b"hello", None, 6, true, None, None).unwrap());
    let mut rx = Side::new(Engine::receiver(false, None, None).unwrap());
    run(&mut tx, &mut rx, 20, 0.003, |_, rx| !rx.problems().is_empty());
    assert!(rx.problems()[0].contains("negotiate"));
    assert!(rx.tape.iter().all(|&v| v == 0.0));
}

#[test]
fn test_bursts() {
    let mut tx = Side::new(Engine::tester(6, 8));
    let mut rx = Side::new(Engine::receiver(false, None, None).unwrap());
    run(&mut tx, &mut rx, 40, 0.003, |_, rx| rx.events.iter().filter(|e| matches!(e, Event::Burst { .. })).count() >= 4);
    let bursts: Vec<(usize, usize)> = rx.events.iter().filter_map(|e| if let Event::Burst { level, wrong, .. } = e { Some((*level, *wrong)) } else { None }).collect();
    assert_eq!(bursts.iter().map(|b| b.0).collect::<Vec<_>>(), [6, 7, 8, 6]);
    assert!(bursts.iter().all(|b| b.1 == 0), "{bursts:?}");
}
