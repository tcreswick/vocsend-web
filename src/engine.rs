//! The two sides of a vocsend transfer, written so that nothing ever waits.
//!
//! A browser cannot block: it hands over microphone samples when it has
//! them and plays sound when asked. So this engine is driven from outside.
//! Feed it what the microphone heard and tell it the time; it hands back
//! sound to play and a list of things that happened. It follows the same
//! protocol, packet for packet, as the command-line program.

use std::collections::VecDeque;
use vocsend::crypto::{self, Cipher};
use vocsend::dsp::{Rng, SR};
use vocsend::frame;
use vocsend::modem::flag::{ENCRYPTED, NEGOTIATED, STREAMING};
use vocsend::modem::kind::*;
use vocsend::modem::{self, level, Packet, Receiver, REPLY_SYLLABLES};
use vocsend::payload;
use vocsend::plan::{self, ATTEMPTS, RAMP_LEVELS, RAMP_SYLLABLES, TEST_SYLLABLES};

/// Pause before replying, so the other side has started listening.
const TURNAROUND_MS: f64 = 600.0;
/// How long after our own sound should have ended we stay deaf. A browser's
/// output runs a little behind, and we would only hear ourselves.
const DEAF_MARGIN_MS: f64 = 400.0;
const PROBE_SYLLABLES: usize = 60;

/// What a finished transfer brought, and how it travelled.
#[derive(Clone, Debug, Default)]
pub struct Received {
    pub data: Vec<u8>,
    pub name: Option<String>,
    pub level: usize,
    pub protocol: u8,
    pub compressed: bool,
    /// Bytes on the air before decompression (and after decryption).
    pub carried: usize,
    pub encrypted: bool,
    pub addressed: bool,
    pub negotiated: bool,
    pub blocks: usize,
    /// Bytes the error correction had to repair.
    pub repaired: usize,
    /// Blocks that had to be sent again.
    pub retries: usize,
    pub fingerprint: String,
    pub seconds: f64,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A line for the log.
    Log(String),
    /// What is happening right now, for a status line.
    Status(String),
    /// Sound was just handed over to be played, lasting this long.
    Speaking { seconds: f64, what: String },
    /// Progress through something long.
    Progress { label: String, fraction: f64 },
    /// The two sides agreed on a level.
    Agreed { level: usize, redundancy: f64, bits_per_second: f64 },
    /// One test burst was heard.
    Burst { level: usize, wrong: usize, of: usize, loudness: f64 },
    /// A transfer arrived intact.
    Received(Box<Received>),
    /// Something went wrong that the user should see.
    Problem(String),
    /// The session is over.
    Done { ok: bool, text: String },
}

enum Step {
    Pending,
    Done,
    TimedOut,
}

/// A stretch of syllables being read, a few at a time as audio arrives.
struct Run {
    /// 0 is the control alphabet.
    lv: usize,
    want: usize,
    heard: Vec<(usize, f32)>,
    deadline: f64,
}

impl Run {
    fn symbols(&self) -> Vec<usize> {
        self.heard.iter().map(|s| s.0).collect()
    }
}

enum RxState {
    Listening,
    Ramp { hi: usize, lv: usize, errors: Vec<usize>, run: Run },
    TestSize { lv: usize, run: Run },
    TestBurst { lv: usize, run: Run },
    FrameHead { p: Packet, run: Run },
    FrameBody { p: Packet, run: Run, length: usize, nsym: usize },
    Probe { lv: usize, run: Run },
}

enum TxState {
    Start,
    AwaitAnswer { tries: usize },
    AwaitReport { tries: usize },
    AwaitAccept,
    SendBlock { attempt: usize },
    AwaitAck { attempt: usize, size: usize },
    Finishing,
}

enum Role {
    Receive {
        state: RxState,
        allow_replies: bool,
        collected: Vec<u8>,
        last_seq: Option<u8>,
        blocks: usize,
        repaired: usize,
        retries: usize,
        started: f64,
    },
    Send(Box<Tx>),
    Test { lo: usize, hi: usize, next: usize, at: f64 },
    /// Only ever in place for a moment, while the sender's state is borrowed.
    Idle,
}

struct Tx {
    state: TxState,
    stream: Vec<u8>,
    original: usize,
    negotiate: bool,
    lv: usize,
    redundancy: f64,
    sent: usize,
    done: usize,
    seq: usize,
    /// When the wait for a reply began: set once our own sound has ended.
    wait_from: Option<f64>,
}

pub struct Engine {
    role: Role,
    rx: Receiver,
    control: Option<Run>,
    cipher: Option<Cipher>,
    address: u8,
    now: f64,
    deaf_until: f64,
    was_deaf: bool,
    /// A reply waiting for its moment.
    pending: Option<(f64, Vec<f32>, String)>,
    audio: VecDeque<Vec<f32>>,
    events: Vec<Event>,
    spoken: bool,
    /// Done, but for a last reply still to be played.
    finishing: bool,
    finished: bool,
}

fn cipher_for(key: Option<&str>) -> Result<Option<Cipher>, String> {
    match key {
        Some(k) if !k.is_empty() => Cipher::new(k).map(Some),
        _ => Ok(None),
    }
}

impl Engine {
    fn new(role: Role, key: Option<&str>, address: Option<&str>) -> Result<Engine, String> {
        let cipher = cipher_for(key)?;
        let address = match (address, &cipher) {
            (Some(a), Some(_)) if !a.is_empty() => crypto::address_tag(a),
            (Some(a), None) if !a.is_empty() => return Err("an address only works together with encryption".into()),
            _ => 0,
        };
        Ok(Engine {
            role,
            rx: Receiver::new(),
            control: None,
            cipher,
            address,
            now: 0.0,
            deaf_until: 0.0,
            was_deaf: false,
            pending: None,
            audio: VecDeque::new(),
            events: Vec::new(),
            spoken: false,
            finishing: false,
            finished: false,
        })
    }

    /// Listen for a transfer or for test bursts. With `allow_replies` it will
    /// answer a sender that negotiates; without, it never makes a sound.
    pub fn receiver(allow_replies: bool, key: Option<&str>, address: Option<&str>) -> Result<Engine, String> {
        let role = Role::Receive {
            state: RxState::Listening,
            allow_replies,
            collected: Vec::new(),
            last_seq: None,
            blocks: 0,
            repaired: 0,
            retries: 0,
            started: 0.0,
        };
        Engine::new(role, key, address)
    }

    /// Send `data`, one-way at `lv` or negotiated.
    pub fn sender(data: &[u8], name: Option<&str>, lv: usize, negotiate: bool, key: Option<&str>, address: Option<&str>) -> Result<Engine, String> {
        let lv = lv.clamp(1, 10);
        let stream = payload::wrap(data, name);
        if stream.len() > 0xFF_FFFF {
            return Err("that is too much to send by sound".into());
        }
        let role = Role::Send(Box::new(Tx {
            state: TxState::Start,
            stream,
            original: data.len(),
            negotiate,
            lv,
            redundancy: level(lv).redundancy,
            sent: 0,
            done: 0,
            seq: 0,
            wait_from: None,
        }));
        Engine::new(role, key, address)
    }

    /// Play test bursts at each level from `lo` to `hi`, round and round.
    pub fn tester(lo: usize, hi: usize) -> Engine {
        let (lo, hi) = (lo.clamp(1, 10), hi.clamp(1, 10));
        let role = Role::Test { lo: lo.min(hi), hi: lo.max(hi), next: lo.min(hi), at: 0.0 };
        Engine::new(role, None, None).expect("no key, no failure")
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The next piece of sound to play, if any. Play it at once.
    pub fn take_audio(&mut self) -> Option<Vec<f32>> {
        self.audio.pop_front()
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// Microphone samples at 16 kHz, with the time now in milliseconds.
    pub fn feed(&mut self, samples: &[f32], now_ms: f64) {
        self.now = now_ms;
        // Deaf while our own sound is in the air.
        if now_ms >= self.deaf_until {
            if self.was_deaf {
                self.rx = Receiver::new();
                self.control = None;
                self.was_deaf = false;
            }
            self.rx.feed(samples);
        }
        self.step();
    }

    /// Let time pass without new audio (a side that only speaks never feeds).
    pub fn tick(&mut self, now_ms: f64) {
        self.now = now_ms;
        self.step();
    }

    fn log(&mut self, text: impl Into<String>) {
        self.events.push(Event::Log(text.into()));
    }

    fn status(&mut self, text: impl Into<String>) {
        self.events.push(Event::Status(text.into()));
    }

    fn finish(&mut self, ok: bool, text: impl Into<String>) {
        self.finished = true;
        self.events.push(Event::Done { ok, text: text.into() });
    }

    fn speaking(&self) -> bool {
        self.now < self.deaf_until
    }

    /// Hand over sound to play now, and go deaf until it is over.
    fn say(&mut self, mut audio: Vec<f32>, what: &str) {
        if !self.spoken {
            // A little faint hiss first: speakers, amplifiers and wireless
            // links need waking, and some go back to sleep on pure silence.
            let mut rng = Rng::new(0);
            let mut lead: Vec<f32> = (0..SR * 8 / 10).map(|_| (0.001 * rng.gaussian()) as f32).collect();
            lead.append(&mut audio);
            audio = lead;
            self.spoken = true;
        }
        let seconds = audio.len() as f64 / SR as f64;
        self.deaf_until = self.now.max(self.deaf_until) + seconds * 1000.0 + DEAF_MARGIN_MS;
        self.was_deaf = true;
        self.events.push(Event::Speaking { seconds, what: what.to_string() });
        self.audio.push_back(audio);
    }

    /// Queue a reply for after the turnaround pause.
    fn reply(&mut self, packet: Packet, what: &str) {
        self.pending = Some((self.now + TURNAROUND_MS, packet.audio(&[]), what.to_string()));
    }

    fn new_run(&mut self, lv: usize, want: usize) -> Run {
        self.rx.begin_run();
        let ms = if lv == 0 { modem::control().ms } else { level(lv).ms };
        Run { lv, want, heard: Vec::with_capacity(want), deadline: self.now + (want * ms) as f64 + 4000.0 }
    }

    fn extend_run(&self, run: &mut Run, want: usize) {
        let ms = if run.lv == 0 { modem::control().ms } else { level(run.lv).ms };
        run.deadline += (want.saturating_sub(run.want) * ms) as f64;
        run.want = want;
    }

    fn advance(&mut self, run: &mut Run) -> Step {
        let alphabet = if run.lv == 0 { modem::control() } else { modem::alphabet(run.lv) };
        while run.heard.len() < run.want {
            match self.rx.next_symbol(alphabet, run.heard.len(), run.want) {
                Some(symbol) => run.heard.push(symbol),
                None => return if self.now > run.deadline { Step::TimedOut } else { Step::Pending },
            }
        }
        Step::Done
    }

    /// Find the next control packet in what has been heard, if there is one yet.
    fn poll_packet(&mut self) -> Option<Packet> {
        loop {
            if self.control.is_none() {
                if !self.rx.search() {
                    return None;
                }
                self.control = Some(self.new_run(0, REPLY_SYLLABLES));
            }
            let mut run = self.control.take().unwrap();
            match self.advance(&mut run) {
                Step::Pending => {
                    self.control = Some(run);
                    return None;
                }
                Step::TimedOut => self.rx.restart(),
                Step::Done => {
                    // The kind comes first, and says how long the packet is.
                    let digits = run.symbols();
                    match Packet::length(&digits) {
                        Some(total) if total > digits.len() => {
                            self.extend_run(&mut run, total);
                            self.control = Some(run);
                            continue;
                        }
                        Some(_) => {
                            if let Some(packet) = Packet::parse(&digits) {
                                return Some(packet);
                            }
                            self.rx.restart();
                        }
                        None => self.rx.restart(),
                    }
                }
            }
        }
    }

    fn step(&mut self) {
        if self.finished {
            return;
        }
        if let Some((due, _, _)) = &self.pending {
            if self.now >= *due {
                let (_, audio, what) = self.pending.take().unwrap();
                self.say(audio, &what);
            }
            return;
        }
        if self.speaking() {
            return;
        }
        if self.finishing {
            return self.finish(true, "Received.");
        }
        match self.role {
            Role::Receive { .. } => self.step_receive(),
            Role::Send(_) => self.step_send(),
            Role::Test { .. } => self.step_test(),
            Role::Idle => {}
        }
    }

    // --------------------------------------------------------------- testing

    fn step_test(&mut self) {
        let Role::Test { lo, hi, next, at } = &mut self.role else { return };
        if self.now < *at {
            return;
        }
        let lv = *next;
        *next = if lv >= *hi { *lo } else { lv + 1 };
        let known = plan::known_symbols(lv, PROBE_SYLLABLES, 3);
        let audio = Packet::sender(PROBE, lv, 0, 0, 0).audio(&[(modem::alphabet(lv), &known)]);
        self.say(audio, &format!("test burst at level {lv}"));
        if let Role::Test { at, .. } = &mut self.role {
            *at = self.deaf_until - DEAF_MARGIN_MS + 300.0;
        }
        // Nothing is listening on this side: no need to stay deaf.
        self.deaf_until -= DEAF_MARGIN_MS;
    }

    // ------------------------------------------------------------- receiving

    fn back_to_listening(&mut self) {
        self.rx.restart();
        if let Role::Receive { state, .. } = &mut self.role {
            *state = RxState::Listening;
        }
    }

    fn step_receive(&mut self) {
        loop {
            let Role::Receive { state, .. } = &mut self.role else { return };
            let state = std::mem::replace(state, RxState::Listening);
            let again = match state {
                RxState::Listening => self.rx_listening(),
                RxState::Ramp { hi, lv, errors, run } => self.rx_ramp(hi, lv, errors, run),
                RxState::TestSize { lv, run } => self.rx_test_size(lv, run),
                RxState::TestBurst { lv, run } => self.rx_test_burst(lv, run),
                RxState::FrameHead { p, run } => self.rx_frame_head(p, run),
                RxState::FrameBody { p, run, length, nsym } => self.rx_frame_body(p, run, length, nsym),
                RxState::Probe { lv, run } => self.rx_probe(lv, run),
            };
            if !again || self.finished || self.pending.is_some() {
                return;
            }
        }
    }

    fn set_rx(&mut self, next: RxState) {
        if let Role::Receive { state, .. } = &mut self.role {
            *state = next;
        }
    }

    fn replies_allowed(&self) -> bool {
        matches!(self.role, Role::Receive { allow_replies: true, .. })
    }

    /// Returns true if there may be more to do straight away.
    fn rx_listening(&mut self) -> bool {
        let Some(p) = self.poll_packet() else { return false };
        if !from_sender(p.kind) {
            self.rx.restart();
            return true;
        }
        if p.address != 0 && p.address != self.address {
            self.log(if self.address == 0 {
                "Heard something addressed to a named receiver; ignoring it."
            } else {
                "Heard something addressed to a different receiver; ignoring it."
            });
            self.rx.restart();
            return true;
        }
        let wants_reply = p.flags & NEGOTIATED != 0 || matches!(p.kind, CALL | RAMP | TEST);
        if p.needs > modem::PROTOCOL || p.version < modem::MIN_SENDER {
            let here = p.needs > modem::PROTOCOL;
            self.events.push(Event::Problem(if here {
                format!(
                    "The sender uses protocol version {} and needs a receiver at version {} or newer; this page speaks version {}. Reload the page to update it.",
                    p.version, p.needs, modem::PROTOCOL
                )
            } else {
                format!(
                    "The sender uses protocol version {}; this page needs version {} or newer. Update the sending side.",
                    p.version, modem::MIN_SENDER
                )
            }));
            if wants_reply && self.replies_allowed() {
                self.reply(Packet::reply(REFUSE, if here { 3 } else { 4 }, 0), "refusing");
            }
            self.rx.restart();
            return true;
        }
        if p.kind == PROBE {
            let lv = p.a as usize;
            if (1..=10).contains(&lv) {
                let run = self.new_run(lv, PROBE_SYLLABLES);
                self.set_rx(RxState::Probe { lv, run });
            } else {
                self.rx.restart();
            }
            return true;
        }
        if wants_reply && !self.replies_allowed() {
            self.events.push(Event::Problem(
                "The sender is trying to negotiate, but this side is set never to transmit. Allow replies here, or send one-way from the other side.".into(),
            ));
            self.rx.restart();
            return true;
        }
        if (p.flags & ENCRYPTED != 0) != self.cipher.is_some() {
            self.events.push(Event::Problem(if self.cipher.is_some() {
                "A key is set here, but the sender is not encrypting. Nothing was accepted.".into()
            } else {
                "The sender is encrypting. Enter the shared key here and listen again.".into()
            }));
            if wants_reply {
                self.reply(Packet::reply(REFUSE, 1, 0), "refusing");
            }
            self.rx.restart();
            return true;
        }
        match p.kind {
            CALL => {
                self.status("The sender is calling. Answering…");
                self.reply(Packet::reply(ANSWER, 0, 0), "answering the call");
                self.rx.restart();
            }
            RAMP => {
                let (lo, hi) = (p.a as usize, p.b as usize);
                if lo >= 1 && hi <= 10 && lo <= hi {
                    self.status("Listening to the speed ramp…");
                    let run = self.new_run(lo, RAMP_SYLLABLES);
                    self.set_rx(RxState::Ramp { hi, lv: lo, errors: Vec::new(), run });
                } else {
                    self.rx.restart();
                }
            }
            TEST => {
                let lv = p.a as usize;
                if (1..=10).contains(&lv) {
                    self.status(format!("Testing level {lv}…"));
                    let run = self.new_run(0, plan::SIZE_SYLLABLES);
                    self.set_rx(RxState::TestSize { lv, run });
                } else {
                    self.rx.restart();
                }
            }
            DATA => {
                let lv = p.a as usize;
                if (1..=10).contains(&lv) {
                    let now = self.now;
                    if let Role::Receive { blocks, started, .. } = &mut self.role {
                        if *blocks == 0 {
                            *started = now;
                        }
                    }
                    self.status(format!("Receiving at level {lv}…"));
                    let run = self.new_run(lv, modem::syllables_for(frame::HEADER_LEN, modem::alphabet(lv).bits()));
                    self.set_rx(RxState::FrameHead { p, run });
                } else {
                    self.rx.restart();
                }
            }
            BYE => {
                if wants_reply {
                    self.reply(Packet::reply(ACK, 0, 0), "acknowledging");
                }
                self.finish(true, "The sender has finished.");
            }
            _ => self.rx.restart(),
        }
        true
    }

    fn rx_ramp(&mut self, hi: usize, lv: usize, mut errors: Vec<usize>, mut run: Run) -> bool {
        match self.advance(&mut run) {
            Step::Pending => {
                self.set_rx(RxState::Ramp { hi, lv, errors, run });
                return false;
            }
            Step::TimedOut => errors.push(RAMP_SYLLABLES),
            Step::Done => {
                let want = plan::known_symbols(lv, RAMP_SYLLABLES, 1);
                errors.push(run.heard.iter().zip(&want).filter(|(g, w)| g.0 != **w).count());
            }
        }
        if lv < hi {
            let run = self.new_run(lv + 1, RAMP_SYLLABLES);
            self.set_rx(RxState::Ramp { hi, lv: lv + 1, errors, run });
            return true;
        }
        let lo = hi + 1 - errors.len();
        let clean = (lo..=hi).zip(&errors).filter(|(_, &e)| e == 0).map(|(l, _)| l).max().unwrap_or(0);
        let summary: Vec<String> = (lo..=hi).zip(&errors).map(|(l, e)| format!("{l}:{e}")).collect();
        self.log(format!("Ramp, wrong syllables per level: {}", summary.join(" ")));
        self.reply(Packet::reply(REPORT, clean, 0), "reporting on the ramp");
        self.back_to_listening();
        true
    }

    fn rx_test_size(&mut self, lv: usize, mut run: Run) -> bool {
        match self.advance(&mut run) {
            Step::Pending => {
                self.set_rx(RxState::TestSize { lv, run });
                false
            }
            // The size is only for showing progress: carry on without it.
            Step::TimedOut | Step::Done => {
                let run = self.new_run(lv, TEST_SYLLABLES);
                self.set_rx(RxState::TestBurst { lv, run });
                true
            }
        }
    }

    fn rx_test_burst(&mut self, lv: usize, mut run: Run) -> bool {
        let wrong = match self.advance(&mut run) {
            Step::Pending => {
                self.set_rx(RxState::TestBurst { lv, run });
                return false;
            }
            Step::TimedOut => TEST_SYLLABLES,
            Step::Done => {
                let want = plan::known_symbols(lv, TEST_SYLLABLES, 2);
                run.heard.iter().zip(&want).filter(|(g, w)| g.0 != **w).count()
            }
        };
        let (use_lv, redundancy) = plan::choose(lv, wrong);
        self.log(format!("Test at level {lv}: {wrong} of {TEST_SYLLABLES} syllables wrong."));
        self.events.push(Event::Agreed { level: use_lv, redundancy, bits_per_second: modem::speed(use_lv, redundancy) });
        self.reply(Packet::reply(ACCEPT, use_lv, plan::redundancy_code(redundancy)), "agreeing the level");
        self.back_to_listening();
        true
    }

    fn rx_probe(&mut self, lv: usize, mut run: Run) -> bool {
        let wrong = match self.advance(&mut run) {
            Step::Pending => {
                self.set_rx(RxState::Probe { lv, run });
                return false;
            }
            Step::TimedOut => PROBE_SYLLABLES,
            Step::Done => {
                let known = plan::known_symbols(lv, PROBE_SYLLABLES, 3);
                run.heard.iter().zip(&known).filter(|(g, w)| g.0 != **w).count()
            }
        };
        let loudness = self.rx.level_db(PROBE_SYLLABLES * modem::alphabet(lv).n) as f64;
        self.events.push(Event::Burst { level: lv, wrong, of: PROBE_SYLLABLES, loudness });
        self.back_to_listening();
        true
    }

    fn block_failed(&mut self, p: Packet, why: &str) -> bool {
        let lv = p.a;
        self.log(format!("A block at level {lv} {why}."));
        if p.flags & NEGOTIATED != 0 {
            if let Role::Receive { retries, .. } = &mut self.role {
                *retries += 1;
            }
            self.reply(Packet::reply(NAK, (p.b & 7) as usize, 0), "asking for it again");
        } else {
            self.events.push(Event::Problem(
                "The message did not arrive intact. One-way transfers cannot be repeated automatically: send it again, perhaps at a lower level.".into(),
            ));
        }
        self.back_to_listening();
        true
    }

    fn rx_frame_head(&mut self, p: Packet, mut run: Run) -> bool {
        match self.advance(&mut run) {
            Step::Pending => {
                self.set_rx(RxState::FrameHead { p, run });
                false
            }
            Step::TimedOut => self.block_failed(p, "did not arrive"),
            Step::Done => {
                let bits = modem::alphabet(run.lv).bits();
                let (bytes, conf) = modem::unpack(&run.heard, bits, frame::HEADER_LEN);
                match frame::decode_header(&bytes, &conf) {
                    None => self.block_failed(p, "could not be read"),
                    Some((length, nsym)) => {
                        let total = frame::HEADER_LEN + frame::body_len(length, nsym);
                        self.extend_run(&mut run, modem::syllables_for(total, bits));
                        self.set_rx(RxState::FrameBody { p, run, length, nsym });
                        true
                    }
                }
            }
        }
    }

    fn rx_frame_body(&mut self, p: Packet, mut run: Run, length: usize, nsym: usize) -> bool {
        match self.advance(&mut run) {
            Step::Pending => {
                let fraction = run.heard.len() as f64 / run.want as f64;
                if run.heard.len() % 8 == 0 {
                    self.events.push(Event::Progress { label: "Receiving".into(), fraction });
                }
                self.set_rx(RxState::FrameBody { p, run, length, nsym });
                return false;
            }
            Step::TimedOut => return self.block_failed(p, "stopped part-way"),
            Step::Done => {}
        }
        let bits = modem::alphabet(run.lv).bits();
        let total = frame::HEADER_LEN + frame::body_len(length, nsym);
        let (bytes, conf) = modem::unpack(&run.heard, bits, total);
        let Some(body) = frame::decode_body(&bytes[frame::HEADER_LEN..], &conf[frame::HEADER_LEN..], length, nsym) else {
            return self.block_failed(p, "had more errors than could be repaired");
        };
        let fixed = frame::repaired(&bytes, &body, nsym);
        let wants_reply = p.flags & NEGOTIATED != 0;
        let (seq, last) = (p.b & 7, p.b & 8 != 0);
        let block = match &mut self.cipher {
            Some(cipher) => match cipher.open(&body, &[p.b, p.flags, p.address]) {
                Ok(plain) => plain,
                Err(why) => {
                    self.events.push(Event::Problem(format!("Could not decrypt: {why}.")));
                    if wants_reply {
                        self.reply(Packet::reply(REFUSE, 2, 0), "refusing");
                    }
                    self.back_to_listening();
                    return true;
                }
            },
            None => body,
        };
        let now = self.now;
        let Role::Receive { collected, last_seq, blocks, repaired, retries, started, .. } = &mut self.role else { return false };
        let repeat = *last_seq == Some(seq) && wants_reply;
        if !repeat {
            collected.extend_from_slice(&block);
            *blocks += 1;
            *repaired += fixed;
            *last_seq = Some(seq);
        }
        let finished = (last && !repeat).then(|| {
            let stream = std::mem::take(collected);
            let summary = (*blocks, *repaired, *retries, (now - *started) / 1000.0);
            *blocks = 0;
            *repaired = 0;
            *retries = 0;
            (stream, summary)
        });
        if wants_reply {
            self.reply(Packet::reply(ACK, seq as usize, 0), "acknowledging");
        }
        self.events.push(Event::Progress { label: "Receiving".into(), fraction: 1.0 });
        let Some((stream, (blocks, repaired, retries, seconds))) = finished else {
            self.back_to_listening();
            return true;
        };
        match payload::unwrap(&stream) {
            Err(why) => self.events.push(Event::Problem(why.to_string())),
            Ok((name, data)) => {
                let received = Received {
                    fingerprint: crypto::fingerprint(&data),
                    name: name.as_deref().map(payload::safe_name),
                    level: p.a as usize,
                    protocol: p.version,
                    compressed: payload::is_compressed(&stream),
                    carried: stream.len() - payload::header_len(&stream),
                    encrypted: p.flags & ENCRYPTED != 0,
                    addressed: p.address != 0,
                    negotiated: wants_reply,
                    blocks,
                    repaired,
                    retries,
                    seconds,
                    data,
                };
                self.events.push(Event::Received(Box::new(received)));
                if p.flags & STREAMING == 0 {
                    // Let the acknowledgement out before stopping.
                    self.back_to_listening();
                    self.finish_after_reply();
                    return false;
                }
            }
        }
        self.back_to_listening();
        true
    }

    fn finish_after_reply(&mut self) {
        // `step` plays a pending reply before anything else; mark the session
        // done only once that has gone out.
        if self.pending.is_none() {
            self.finish(true, "Received.");
        } else {
            self.finishing = true;
        }
    }

    // --------------------------------------------------------------- sending

    fn refused(&mut self, reason: u8) {
        let encrypting = self.cipher.is_some();
        let text = match reason {
            1 if encrypting => "The receiver refused: this side is encrypting, but the receiver has no key set.",
            1 => "The receiver refused: it has a key set, but this side is not encrypting.",
            2 => "The receiver refused: the keys do not match.",
            3 => "The receiver refused: it is too old for this page. Update the receiving side.",
            4 => "The receiver refused: this page is too old for it. Reload to update.",
            _ => "The receiver refused the transfer.",
        };
        self.finish(false, text);
    }

    /// Speak after the turnaround pause, then wait for a reply.
    fn say_then_wait(&mut self, tx: &mut Tx, audio: Vec<f32>, what: &str, next: TxState) {
        self.pending = Some((self.now + TURNAROUND_MS, audio, what.to_string()));
        tx.state = next;
        tx.wait_from = None;
    }

    fn step_send(&mut self) {
        let mut tx = match std::mem::replace(&mut self.role, Role::Idle) {
            Role::Send(tx) => tx,
            other => {
                self.role = other;
                return;
            }
        };
        self.drive(&mut tx);
        self.role = Role::Send(tx);
    }

    fn drive(&mut self, tx: &mut Tx) {
        let encrypted = self.cipher.is_some();
        let flags = (if encrypted { ENCRYPTED } else { 0 }) | (if tx.negotiate { NEGOTIATED } else { 0 });
        let address = self.address;
        let now = self.now;
        // This only runs once our own sound has ended: that is when waiting starts.
        let waited = now - *tx.wait_from.get_or_insert(now);
        match tx.state {
            TxState::Start => {
                if tx.negotiate {
                    self.status("Calling the receiver…");
                    self.say(Packet::sender(CALL, 0, 0, flags, address).audio(&[]), "calling");
                    tx.state = TxState::AwaitAnswer { tries: 1 };
                    tx.wait_from = None;
                } else {
                    tx.state = TxState::SendBlock { attempt: 1 };
                    self.drive(tx);
                }
            }
            TxState::AwaitAnswer { tries } => match self.poll_packet() {
                Some(p) if p.kind == REFUSE => self.refused(p.a),
                Some(p) if p.kind == ANSWER => {
                    self.status("The receiver answered. Playing the speed ramp…");
                    self.send_ramp(tx, flags, 1);
                }
                Some(_) => self.rx.restart(),
                None if waited > 6000.0 => {
                    if tries >= 40 {
                        self.finish(false, "Nobody answered. Is the other side listening, with replies allowed, and can it hear this device?");
                    } else {
                        self.say(Packet::sender(CALL, 0, 0, flags, address).audio(&[]), "calling");
                        tx.state = TxState::AwaitAnswer { tries: tries + 1 };
                        tx.wait_from = None;
                    }
                }
                None => {}
            },
            TxState::AwaitReport { tries } => match self.poll_packet() {
                Some(p) if p.kind == REFUSE => self.refused(p.a),
                Some(p) if p.kind == REPORT => {
                    let clean = p.a as usize;
                    let try_level = if (1..=10).contains(&clean) { clean } else { 2 };
                    self.log(if clean == 0 {
                        "No ramp level arrived clean; testing level 2.".to_string()
                    } else {
                        format!("The receiver heard level {try_level} clean; testing it.")
                    });
                    self.status(format!("Testing level {try_level}…"));
                    tx.lv = try_level;
                    let size = plan::size_digits(tx.stream.len());
                    let known = plan::known_symbols(try_level, TEST_SYLLABLES, 2);
                    let audio = Packet::sender(TEST, try_level, 0, flags, address)
                        .audio(&[(modem::control(), &size), (modem::alphabet(try_level), &known)]);
                    self.say_then_wait(tx, audio, "testing", TxState::AwaitAccept);
                }
                Some(_) => self.rx.restart(),
                None if waited > 10000.0 => {
                    if tries >= ATTEMPTS {
                        self.finish(false, "The receiver answered the call but sent no report after the ramp.");
                    } else {
                        self.send_ramp(tx, flags, tries + 1);
                    }
                }
                None => {}
            },
            TxState::AwaitAccept => {
                let accepted = match self.poll_packet() {
                    Some(p) if p.kind == REFUSE => return self.refused(p.a),
                    Some(p) if p.kind == ACCEPT && (1..=10).contains(&p.a) => {
                        Some((p.a as usize, plan::REDUNDANCY[(p.b as usize).min(plan::REDUNDANCY.len() - 1)]))
                    }
                    Some(_) => {
                        self.rx.restart();
                        None
                    }
                    None if waited > 10000.0 => {
                        let lower = tx.lv.saturating_sub(1).max(1);
                        Some((lower, level(lower).redundancy))
                    }
                    None => None,
                };
                if let Some((lv, redundancy)) = accepted {
                    tx.lv = lv;
                    tx.redundancy = redundancy;
                    tx.state = TxState::SendBlock { attempt: 1 };
                    self.events.push(Event::Agreed { level: lv, redundancy, bits_per_second: modem::speed(lv, redundancy) });
                    self.pause();
                }
            }
            TxState::SendBlock { attempt } => {
                let left_bytes = tx.stream.len() - tx.sent;
                let size = plan::split(left_bytes, tx.lv, tx.redundancy, tx.negotiate, encrypted)[0];
                let chunk = &tx.stream[tx.sent..tx.sent + size];
                let last = tx.sent + size >= tx.stream.len();
                let field = (tx.seq & 7) | (last as usize) << 3;
                let (more, left) = plan::estimate(left_bytes, tx.lv, tx.redundancy, tx.negotiate, encrypted);
                let label = format!("Block {} of {}", tx.done + 1, tx.done + more);
                let body = match &mut self.cipher {
                    Some(c) => match c.seal(chunk, &[field as u8, flags, address]) {
                        Ok(sealed) => sealed,
                        Err(why) => return self.finish(false, why),
                    },
                    None => chunk.to_vec(),
                };
                let alphabet = modem::alphabet(tx.lv);
                let symbols = modem::pack(&frame::encode(&body, tx.redundancy), alphabet.bits());
                let audio = Packet::sender(DATA, tx.lv, field, flags, address).audio(&[(alphabet, &symbols)]);
                self.status(format!(
                    "{label} at level {}, about {} left{}",
                    tx.lv,
                    plan::clock(left),
                    if attempt > 1 { format!(" (attempt {attempt})") } else { String::new() }
                ));
                self.say(audio, &label);
                tx.wait_from = None;
                if tx.negotiate {
                    tx.state = TxState::AwaitAck { attempt, size };
                } else {
                    tx.sent += size;
                    tx.done += 1;
                    tx.seq += 1;
                    tx.state = if last { TxState::Finishing } else { TxState::SendBlock { attempt: 1 } };
                }
            }
            TxState::AwaitAck { attempt, size } => {
                let want = (tx.seq & 7) as u8;
                let outcome = match self.poll_packet() {
                    Some(p) if p.kind == REFUSE => return self.refused(p.a),
                    Some(p) if p.kind == ACK && p.a == want => Some(true),
                    Some(p) if p.kind == NAK => Some(false),
                    Some(_) => {
                        self.rx.restart();
                        None
                    }
                    None if waited > 11000.0 => Some(false),
                    None => None,
                };
                match outcome {
                    None => {}
                    Some(true) => {
                        tx.sent += size;
                        tx.done += 1;
                        tx.seq += 1;
                        if tx.sent >= tx.stream.len() {
                            self.finish(true, format!("Delivered {} bytes at level {}.", tx.original, tx.lv));
                        } else {
                            tx.state = TxState::SendBlock { attempt: 1 };
                            self.pause();
                        }
                    }
                    Some(false) => {
                        if attempt >= ATTEMPTS {
                            self.finish(false, format!("Gave up after {ATTEMPTS} attempts at a block."));
                        } else {
                            tx.lv = tx.lv.saturating_sub(1).max(1);
                            tx.redundancy = tx.redundancy.max(level(tx.lv).redundancy);
                            tx.state = TxState::SendBlock { attempt: attempt + 1 };
                            self.log("That block was not acknowledged; trying again one level slower.");
                            self.pause();
                        }
                    }
                }
            }
            TxState::Finishing => {
                self.finish(true, format!(
                    "Sent {} bytes at level {}. One-way: nothing confirms that it arrived.",
                    tx.original, tx.lv
                ));
            }
        }
    }

    /// A short silence before speaking again, so the other side is listening.
    fn pause(&mut self) {
        self.deaf_until = self.deaf_until.max(self.now + TURNAROUND_MS);
    }

    fn send_ramp(&mut self, tx: &mut Tx, flags: u8, tries: usize) {
        let (lo, hi) = RAMP_LEVELS;
        let bursts: Vec<Vec<usize>> = (lo..=hi).map(|lv| plan::known_symbols(lv, RAMP_SYLLABLES, 1)).collect();
        let sections: Vec<(&vocsend::synth::Alphabet, &[usize])> =
            (lo..=hi).zip(&bursts).map(|(lv, b)| (modem::alphabet(lv), b.as_slice())).collect();
        let audio = Packet::sender(RAMP, lo, hi, flags, self.address).audio(&sections);
        self.say_then_wait(tx, audio, "the speed ramp", TxState::AwaitReport { tries });
    }
}
