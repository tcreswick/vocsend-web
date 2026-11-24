// vocsend in the browser: the page. It handles the screen, the speaker and
// the microphone; the protocol itself runs in worker.js.

const $ = id => document.getElementById(id);
const worker = new Worker("./worker.js", { type: "module" });

let levels = [];                 // filled in when the engine is ready
let active = null;               // the running session: { view, mode }
let context = null;              // the AudioContext, made on first use
let mic = null;                  // { stream, source, node }
let nextPlay = 0;                // when the speaker is next free
let wakeLock = null;
let pending = null;              // data handed from Receive to Send: { bytes, name }
let lastReceived = null;
const estimates = new Map();
let estimateId = 0;

// ------------------------------------------------------------------- bars

for (const bars of document.querySelectorAll(".bars")) {
  for (let i = 0; i < 28; i++) bars.appendChild(document.createElement("span"));
}

/** Show a level from 0 to 1 on a view's bars, with some life in it. */
function drawBars(view, level) {
  const bars = document.querySelector(`[data-view="${view}"] .status .bars`);
  if (!bars) return;
  bars.classList.remove("idle");
  let i = 0;
  for (const span of bars.children) {
    const shape = 0.55 + 0.45 * Math.sin(i++ * 0.9 + performance.now() / 130);
    span.style.height = Math.max(8, Math.min(100, 100 * level * shape * (0.6 + 0.8 * Math.random()))) + "%";
  }
}

function restBars(view) {
  const bars = document.querySelector(`[data-view="${view}"] .status .bars`);
  if (!bars) return;
  for (const span of bars.children) span.style.height = "";
  bars.classList.add("idle");
}

// ------------------------------------------------------------------ sound

async function audio() {
  if (!context) context = new (window.AudioContext || window.webkitAudioContext)();
  if (context.state !== "running") await context.resume();
  return context;
}

/** Play 16 kHz samples as soon as the speaker is free. */
function play(samples) {
  if (!context) return;
  const buffer = context.createBuffer(1, samples.length, 16000);
  buffer.getChannelData(0).set(samples);
  const source = context.createBufferSource();
  source.buffer = buffer;
  source.connect(context.destination);
  const at = Math.max(context.currentTime + 0.05, nextPlay);
  source.start(at);
  nextPlay = at + buffer.duration;
}

async function startMic(view) {
  if (!navigator.mediaDevices?.getUserMedia) {
    throw new Error("This browser will not give a page the microphone. The page must be opened over https.");
  }
  const ctx = await audio();
  // Ask for the microphone raw. Phones do not always oblige, which is what
  // the vocal-style signal is designed to survive.
  const stream = await navigator.mediaDevices.getUserMedia({
    audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false, channelCount: 1 },
  });
  const settings = stream.getAudioTracks()[0].getSettings();
  const say = v => (v === undefined ? "not reported" : v ? "on" : "off");
  log(view, `Microphone at ${ctx.sampleRate} Hz. Browser reports echo cancellation ${say(settings.echoCancellation)}, ` +
    `noise suppression ${say(settings.noiseSuppression)}, automatic gain ${say(settings.autoGainControl)}.`);
  const source = ctx.createMediaStreamSource(stream);
  const handle = input => {
    let peak = 0;
    for (let i = 0; i < input.length; i += 4) peak = Math.max(peak, Math.abs(input[i]));
    if (active && performance.now() > (active.speakingUntil || 0)) {
      drawBars(active.view, Math.min(1, Math.max(0.06, 1 + Math.log10(peak + 1e-5) / 2.2)));
    }
    worker.postMessage({ type: "audio", samples: input, rate: ctx.sampleRate }, [input.buffer]);
  };
  let node;
  try {
    await ctx.audioWorklet.addModule("./tap.js");
    node = new AudioWorkletNode(ctx, "tap");
    node.port.onmessage = e => handle(e.data);
    source.connect(node);
  } catch {
    node = ctx.createScriptProcessor(4096, 1, 1);
    node.onaudioprocess = e => handle(Float32Array.from(e.inputBuffer.getChannelData(0)));
    source.connect(node);
    node.connect(ctx.destination);
  }
  mic = { stream, source, node };
}

function stopMic() {
  if (!mic) return;
  mic.stream.getTracks().forEach(t => t.stop());
  try { mic.source.disconnect(); mic.node.disconnect(); } catch { /* already gone */ }
  mic = null;
}

async function keepAwake(on) {
  try {
    if (on && navigator.wakeLock) wakeLock = await navigator.wakeLock.request("screen");
    else if (!on && wakeLock) { await wakeLock.release(); wakeLock = null; }
  } catch { /* not essential */ }
}

// ----------------------------------------------------------------- helpers

function log(view, text) {
  const box = $(`${view}-log`);
  if (!box) return;
  box.textContent += text + "\n";
  box.scrollTop = box.scrollHeight;
}

function notice(view, text, good, print, after) {
  const box = $(`${view}-result`);
  const div = document.createElement("div");
  div.className = "notice " + (good ? "good" : "bad");
  div.textContent = text;
  if (print) {
    const mark = document.createElement("b");
    mark.className = "print";
    mark.textContent = print;
    div.append(mark, after || "");
  }
  box.appendChild(div);
}

function clock(seconds) {
  const s = Math.round(seconds);
  return s >= 60 ? `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s` : `${s}s`;
}

async function fingerprint(bytes) {
  if (!crypto.subtle) return "";
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  // The same four characters the command line prints: 20 bits of the hash.
  const bits = hash[0] << 12 | hash[1] << 4 | hash[2] >> 4;
  return [3, 2, 1, 0].map(i => "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[(bits >> (5 * i)) & 31]).join("");
}

function setBar(view, fraction) {
  const bar = $(`${view}-bar`);
  if (bar) bar.style.width = `${Math.max(0, Math.min(1, fraction)) * 100}%`;
}

/** Animate a view while this device is speaking for `seconds`. */
function speaking(view, seconds, showProgress) {
  const started = performance.now();
  if (active) active.speakingUntil = started + seconds * 1000;
  const frame = () => {
    if (!active || active.view !== view) return;
    const t = (performance.now() - started) / 1000;
    if (t >= seconds) { if (!mic) restBars(view); return; }
    drawBars(view, 0.75);
    if (showProgress) setBar(view, t / seconds);
    requestAnimationFrame(frame);
  };
  requestAnimationFrame(frame);
}

// ------------------------------------------------------------- start, stop

const BUTTONS = {
  send: ["Send now", "Stop", "send"],
  receive: ["Start listening", "Stop listening", "receive"],
  test: ["Start", "Stop", "receive"],
};

function setButton(view, running) {
  const button = $(`${view}-go`);
  const [idle, busy, colour] = BUTTONS[view];
  button.textContent = running ? busy : view === "test" ? testLabel() : idle;
  button.className = "action " + (running ? "stop" : colour);
}

async function start(view, mode, options, needsMic) {
  $(`${view}-result`).textContent = "";
  $(`${view}-status`).hidden = false;
  setBar(view, 0);
  active = { view, mode };
  setButton(view, true);
  try {
    await audio();
    nextPlay = 0;
    if (needsMic) await startMic(view);
  } catch (error) {
    stop();
    const denied = error.name === "NotAllowedError" || error.name === "SecurityError";
    notice(view, denied ? "The microphone was not allowed. Allow it for this page and try again." : error.message, false);
    return;
  }
  keepAwake(true);
  restBars(view);
  worker.postMessage({ type: "start", mode, options });
}

function stop() {
  worker.postMessage({ type: "stop" });
  stopMic();
  keepAwake(false);
  if (context && nextPlay > context.currentTime + 0.2) {
    // Cut off anything still queued for the speaker.
    context.close();
    context = null;
  }
  if (active) {
    restBars(active.view);
    setButton(active.view, false);
    $(`${active.view}-now`).textContent = "";
    $(`${active.view}-status`).hidden = true;
  }
  active = null;
}

// ------------------------------------------------------- what the engine says

worker.onmessage = ({ data }) => {
  if (data.type === "ready") {
    levels = data.levels;
    $("build-info").textContent = `Protocol version ${data.protocol}.`;
    fillLevels();
    return;
  }
  if (data.type === "estimate") {
    estimates.get(data.id)?.(data.seconds);
    estimates.delete(data.id);
    return;
  }
  if (data.type === "play") return play(data.samples);
  if (data.type !== "events" || !active) return;
  const view = active.view;
  for (const event of data.events) {
    switch (event.type) {
      case "log": log(view, event.text); break;
      case "status": $(`${view}-now`).textContent = event.text; log(view, event.text); break;
      case "speaking":
        speaking(view, event.seconds, view === "send" && /^Block/.test(event.what));
        if (view === "test") $("test-now").textContent = `Playing a ${event.what}…`;
        break;
      case "progress": setBar(view, event.fraction); break;
      case "agreed":
        $(`${view}-detail`).textContent = `Agreed on level ${event.level} with ${Math.round(event.redundancy * 100)}% error correction, about ${Math.round(event.bitsPerSecond)} bits/s.`;
        log(view, $(`${view}-detail`).textContent);
        break;
      case "burst": onBurst(event); break;
      case "received": onReceived(event); break;
      case "problem": notice(view, event.text, false); log(view, event.text); break;
      case "done": onDone(view, event); break;
    }
  }
};

function onDone(view, event) {
  log(view, event.text);
  // Let the last of the sound out before the microphone and speaker are released.
  const wait = context ? Math.max(0, nextPlay - context.currentTime) * 1000 + 150 : 0;
  const finishing = active;
  setTimeout(async () => {
    if (active !== finishing) return;
    setBar(view, event.ok ? 1 : 0);
    stop();
    if (view === "send" && event.ok && sendPayload.last) {
      notice(view, `${event.text} Fingerprint `, true, await fingerprint(sendPayload.last), " — the receiver should show the same.");
    } else if (!(view === "receive" && event.ok)) {
      notice(view, event.text, event.ok);
    }
  }, wait);
}

// --------------------------------------------------------------------- send

const sendPayload = { last: null };

function currentPayload() {
  if (pending) return pending;
  return { bytes: new TextEncoder().encode($("send-text").value), name: null };
}

function describeLevel() {
  const n = +$("send-level").value, l = levels[n - 1];
  const negotiate = $("send-negotiate").checked;
  $("send-level").disabled = negotiate;
  $("send-level-block").classList.toggle("off", negotiate);
  $("send-level-title").firstChild.textContent = negotiate ? "Level: automatic" : "Level ";
  $("send-level-n").hidden = negotiate;
  $("send-level-n").textContent = n;
  if (negotiate) $("send-level-about").textContent = "Negotiate is on, so the two devices pick the fastest level that works";
  else if (l) $("send-level-about").textContent = `${l.pitches} pitches · ${l.ms} ms syllables · about ${Math.round(l.bitsPerSecond)} bits/s`;
}

function refreshSend() {
  const { bytes } = currentPayload();
  $("send-chip").hidden = !pending;
  $("send-text").hidden = !!pending;
  if (pending) $("send-chip-text").textContent = `${pending.name || "Received data"} · ${pending.bytes.length} bytes`;
  $("send-key-box").hidden = !$("send-encrypt").checked;
  describeLevel();
  if (!bytes.length) { $("send-size").textContent = "Nothing to send yet."; return; }
  const negotiate = $("send-negotiate").checked, id = ++estimateId;
  estimates.set(id, seconds => {
    if (id !== estimateId) return;
    $("send-size").textContent = negotiate
      ? `${bytes.length} bytes · speed is agreed with the receiver`
      : `${bytes.length} bytes · about ${clock(seconds)} at level ${$("send-level").value}`;
  });
  worker.postMessage({ type: "estimate", id, bytes: bytes.length, level: +$("send-level").value, negotiate, encrypted: $("send-encrypt").checked });
}

for (const id of ["send-text", "send-level", "send-negotiate", "send-encrypt"]) $(id).addEventListener("input", refreshSend);
$("send-chip-clear").onclick = () => { pending = null; refreshSend(); };
$("send-file").onchange = async e => {
  const file = e.target.files[0];
  if (!file) return;
  pending = { bytes: new Uint8Array(await file.arrayBuffer()), name: file.name };
  e.target.value = "";
  refreshSend();
};

$("send-go").onclick = () => {
  if (active) return stop();
  const { bytes, name } = currentPayload();
  if (!bytes.length) return notice("send", "There is nothing to send yet.", false);
  const encrypt = $("send-encrypt").checked, key = $("send-key").value;
  if (encrypt && !key) { $("send-settings").open = true; return notice("send", "Enter a key, or switch encryption off.", false); }
  const negotiate = $("send-negotiate").checked;
  sendPayload.last = bytes;
  $("send-detail").textContent = "";
  $("send-now").textContent = negotiate ? "Starting…" : "Sending…";
  start("send", "send", {
    data: bytes, name, level: +$("send-level").value, negotiate,
    key: encrypt ? key : null, address: encrypt ? $("send-address").value.trim() || null : null,
  }, negotiate);
};

// ------------------------------------------------------------------ receive

$("receive-encrypt").addEventListener("input", () => { $("receive-key-box").hidden = !$("receive-encrypt").checked; });

$("receive-go").onclick = () => {
  if (active) return stop();
  const encrypt = $("receive-encrypt").checked, key = $("receive-key").value;
  if (encrypt && !key) { $("receive-settings").open = true; return notice("receive", "Enter the key, or switch Encrypted off.", false); }
  $("receive-now").textContent = "Listening… start the sender now.";
  $("receive-detail").textContent = $("receive-replies").checked ? "" : "One-way: this device will not make a sound.";
  start("receive", "receive", {
    replies: $("receive-replies").checked,
    key: encrypt ? key : null, address: encrypt ? $("receive-address").value.trim() || null : null,
  }, true);
};

function onReceived(event) {
  if (active?.view !== "receive") return;
  const bytes = event.data;
  let text = null;
  try { text = new TextDecoder("utf-8", { fatal: true }).decode(bytes); } catch { /* not text */ }
  if (text !== null && /[\u0000-\u0008\u000e-\u001f]/.test(text)) text = null;
  lastReceived = { bytes, name: event.name, text };
  $("received").hidden = false;
  $("receive-text").value = text ?? `(${bytes.length} bytes of binary data${event.name ? `: ${event.name}` : ""}. Use "Save as file".)`;
  $("receive-copy").disabled = text === null;
  const l = levels[event.level - 1];
  const rows = [
    ["File name", event.name || "none sent"],
    ["Size", `${bytes.length} bytes`],
    ["Fingerprint", event.fingerprint],
    ["Level", l ? `${event.level} (${l.pitches} pitches, ${l.ms} ms syllables)` : event.level],
    ["Mode", event.negotiated ? "negotiated, every block acknowledged" : "one-way"],
    ["Compression", event.compressed ? `yes: ${event.carried} bytes on the air` : "none (would not have helped)"],
    ["Encrypted", event.encrypted ? (event.addressed ? "yes, addressed to this receiver" : "yes") : "no"],
    ["Blocks", event.retries ? `${event.blocks}, plus ${event.retries} sent again` : String(event.blocks)],
    ["Errors repaired", event.repaired ? `${event.repaired} bytes fixed by error correction` : "none needed"],
    ["Time", clock(event.seconds)],
    ["Protocol", `version ${event.protocol}`],
  ];
  $("receive-info").innerHTML = "";
  for (const [k, v] of rows) {
    const dt = document.createElement("dt"), dd = document.createElement("dd");
    dt.textContent = k; dd.textContent = v;
    $("receive-info").append(dt, dd);
  }
  notice("receive", `Received ${bytes.length} bytes, checked and intact. Fingerprint `, true, event.fingerprint, " — the sender should show the same.");
  log("receive", `Received ${bytes.length} bytes at level ${event.level}.`);
}

$("receive-copy").onclick = async () => {
  const text = lastReceived?.text;
  if (text == null) return;
  try { await navigator.clipboard.writeText(text); }
  catch { $("receive-text").select(); document.execCommand("copy"); }
  $("receive-copy").textContent = "Copied";
  setTimeout(() => { $("receive-copy").textContent = "Copy"; }, 1500);
};

$("receive-save").onclick = () => {
  if (!lastReceived) return;
  const link = document.createElement("a");
  link.href = URL.createObjectURL(new Blob([lastReceived.bytes]));
  link.download = lastReceived.name || (lastReceived.text !== null ? "received.txt" : "received.bin");
  link.click();
  setTimeout(() => URL.revokeObjectURL(link.href), 5000);
};

$("receive-resend").onclick = () => {
  if (!lastReceived) return;
  if (lastReceived.text !== null && !lastReceived.name) {
    pending = null;
    $("send-text").value = lastReceived.text;
  } else {
    pending = { bytes: lastReceived.bytes, name: lastReceived.name };
  }
  location.hash = "#/send";
};

// --------------------------------------------------------------------- test

let testSends = false;
const bursts = new Map();          // level -> [{ wrong, of, loudness }]
let burstCount = 0;

function testLabel() { return testSends ? "Start sending" : "Start listening"; }

function drawTest() {
  const lo = testSends ? +$("test-lo").value : 1, hi = testSends ? +$("test-hi").value : 10;
  let best = null, html = "";
  for (const l of levels) {
    if (l.level < Math.min(lo, hi) || l.level > Math.max(lo, hi)) continue;
    const runs = bursts.get(l.level) || [];
    const head = `<td>${l.level}</td><td>${Math.round(l.bitsPerSecond)}</td>`;
    if (!runs.length) { html += `<tr>${head}<td class="waiting" colspan="4">${testSends ? "—" : "waiting"}</td></tr>`; continue; }
    const last = runs[runs.length - 1], recent = runs.slice(-3);
    const average = recent.reduce((s, r) => s + r.wrong / r.of, 0) / recent.length;
    const grade = recent[recent.length - 1].verdictOf(average);
    if (grade === "solid") best = l.level;
    html += `<tr>${head}<td>${(100 * last.wrong / last.of).toFixed(1)}%</td><td>${last.loudness.toFixed(0)} dB</td><td>${(100 * average).toFixed(1)}%</td><td class="${grade}">${grade}</td></tr>`;
  }
  $("test-table").innerHTML = html;
  $("test-table-box").hidden = testSends;
  $("test-summary").textContent = testSends ? "The table appears on the listening device."
    : burstCount === 0 ? "" : `Bursts heard: ${burstCount}. ` + (best ? `Fastest solid level so far: ${best}.` : "No level is solid yet.");
}

/** The same judgement the command line makes: can error correction cope? */
function verdictFor(level) {
  const l = levels[level - 1];
  return wrongShare => {
    const repairable = l.redundancy / (2 * (1 + l.redundancy));
    const bad = 1 - (1 - wrongShare) ** (8 / Math.log2(l.pitches));
    return bad <= repairable / 3 ? "solid" : bad <= repairable ? "marginal" : "fails";
  };
}

function onBurst(event) {
  if (active?.view !== "test") {
    if (active?.view === "receive") $("receive-detail").textContent = "Hearing speed-test bursts: the sender is in test mode. Open Speed test to see them.";
    return;
  }
  if (!bursts.has(event.level)) bursts.set(event.level, []);
  bursts.get(event.level).push({ wrong: event.wrong, of: event.of, loudness: event.loudness, verdictOf: verdictFor(event.level) });
  burstCount++;
  $("test-now").textContent = `Heard level ${event.level}: ${event.wrong} of ${event.of} syllables wrong.`;
  log("test", `level ${event.level}: ${event.wrong} of ${event.of} wrong, ${event.loudness.toFixed(0)} dB`);
  drawTest();
}

function setDirection(sends) {
  if (active) stop();
  testSends = sends;
  $("test-listen").setAttribute("aria-pressed", String(!sends));
  $("test-speak").setAttribute("aria-pressed", String(sends));
  $("test-range").hidden = !sends;
  setButton("test", false);
  drawTest();
}
$("test-listen").onclick = () => setDirection(false);
$("test-speak").onclick = () => setDirection(true);
for (const id of ["test-lo", "test-hi"]) $(id).addEventListener("input", drawTest);

$("test-go").onclick = () => {
  if (active) return stop();
  if (testSends) {
    $("test-now").textContent = "Starting…";
    $("test-detail").textContent = "On the other device, listen with Speed test or `vocsend rx test`.";
    start("test", "test", { lo: +$("test-lo").value, hi: +$("test-hi").value }, false);
  } else {
    bursts.clear(); burstCount = 0; drawTest();
    $("test-now").textContent = "Listening for test bursts…";
    $("test-detail").textContent = "On the other device, send with Speed test or `vocsend tx test`.";
    start("test", "receive", { replies: false, key: null, address: null }, true);
  }
};

// ------------------------------------------------------------------ levels

function fillLevels() {
  $("levels-table").innerHTML = levels.map(l =>
    `<tr><td>${l.level}</td><td>${l.pitches}</td><td>${l.ms} ms</td><td>${Math.round(l.redundancy * 100)}%</td><td>${Math.round(l.bitsPerSecond)}</td></tr>`).join("");
  for (const id of ["test-lo", "test-hi"]) $(id).innerHTML = levels.map(l => `<option>${l.level}</option>`).join("");
  $("test-lo").value = 3; $("test-hi").value = 10;
  refreshSend();
  drawTest();
}

// ----------------------------------------------------------------- routing

function route() {
  const view = { "#/send": "send", "#/receive": "receive", "#/test": "test" }[location.hash] || "home";
  if (active && active.view !== view) stop();
  for (const section of document.querySelectorAll("[data-view]")) section.hidden = section.dataset.view !== view;
  if (view === "send") refreshSend();
  if (view === "test") { setButton("test", !!active); drawTest(); }
  window.scrollTo(0, 0);
}
window.addEventListener("hashchange", route);

// ---------------------------------------------------------------- install
// Keep a copy of the app on the device so it works offline, and offer to
// install it where the browser allows.

if ("serviceWorker" in navigator) navigator.serviceWorker.register("./sw.js").catch(() => {});

{
  const installed = matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
  const apple = /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
  let offer = null;
  if (!installed && apple) {
    $("install").hidden = false;
    $("install-button").hidden = true;
    $("install-how").hidden = false;
  }
  window.addEventListener("beforeinstallprompt", event => {
    event.preventDefault();
    offer = event;
    $("install").hidden = false;
  });
  $("install-button").addEventListener("click", async () => {
    if (!offer) return;
    offer.prompt();
    await offer.userChoice;
    offer = null;
    $("install").hidden = true;
  });
  window.addEventListener("appinstalled", () => { $("install").hidden = true; });
}
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible" && active) keepAwake(true); });
route();
