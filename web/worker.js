// The vocsend engine (the Rust core, compiled to WebAssembly) runs here, off
// the page's main thread. The page sends microphone audio in and gets back
// sound to play and a list of things that happened.
import init, { Session, Resampler, levels, estimate, protocol_version } from "./pkg/vocsend_web.js";

let session = null, resampler = null, rate = 0, timer = null;

function drain() {
  if (!session) return;
  for (let audio; (audio = session.take_audio());) postMessage({ type: "play", samples: audio }, [audio.buffer]);
  const events = JSON.parse(session.take_events());
  for (const event of events) if (event.type === "received") event.data = session.take_data();
  if (events.length) postMessage({ type: "events", events });
  if (session.finished()) stop();
}

function stop() {
  clearInterval(timer);
  timer = null;
  if (session) { session.free(); session = null; }
  if (resampler) { resampler.free(); resampler = null; rate = 0; }
}

const ready = init().then(() => postMessage({ type: "ready", levels: JSON.parse(levels()), protocol: protocol_version() }));

onmessage = async ({ data }) => {
  await ready;
  try {
    if (data.type === "start") {
      stop();
      const o = data.options;
      if (data.mode === "receive") session = Session.receiver(o.replies, o.key, o.address);
      else if (data.mode === "send") session = Session.sender(o.data, o.name, o.level, o.negotiate, o.key, o.address);
      else session = Session.tester(o.lo, o.hi);
      // A side that only speaks is never fed audio, so the clock drives it.
      timer = setInterval(() => { if (session) { session.tick(performance.now()); drain(); } }, 50);
      session.tick(performance.now());
      drain();
    } else if (data.type === "audio" && session) {
      if (data.rate !== rate) {
        if (resampler) resampler.free();
        rate = data.rate;
        resampler = rate === 16000 ? null : new Resampler(rate, 16000);
      }
      session.feed(resampler ? resampler.push(data.samples) : data.samples, performance.now());
      drain();
    } else if (data.type === "stop") {
      stop();
    } else if (data.type === "estimate") {
      postMessage({ type: "estimate", id: data.id, seconds: estimate(data.bytes, data.level, data.negotiate, data.encrypted) });
    }
  } catch (error) {
    stop();
    postMessage({ type: "events", events: [{ type: "done", ok: false, text: String(error.message || error) }] });
  }
};
