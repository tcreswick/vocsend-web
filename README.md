# vocsend-web

The web app for [vocsend](https://github.com/tcreswick/vocsend): easily
transmit data between air-gapped devices using audio, straight from a
browser. It is built for phones first, and speaks the same protocol as the
command-line program, so a phone can exchange data with a terminal.

By Tim Creswick. MIT licence.

## What is here

| | |
|---|---|
| `web/` | the site: `index.html` (the app), `cli.html` (the command-line guide), `app.js`, `worker.js`, `style.css` |
| `src/engine.rs` | the send, receive and test logic, written so that nothing ever blocks: a browser feeds it microphone samples and plays what it hands back |
| `src/lib.rs` | the thin layer that exposes the engine to JavaScript |
| `tests/engine.rs` | two engines talking through simulated air |
| `examples/bridge.rs` | runs the engine as an ordinary process, for testing it against the command-line vocsend |

The protocol itself — the sound, the packets, error correction, compression
and encryption — is not here. It comes from the
[vocsend](https://github.com/tcreswick/vocsend) library, which this depends
on, so the web app and the command line cannot drift apart. `Cargo.lock`
pins the version; `cargo update -p vocsend` moves to the latest.

Everything runs in the browser. The page makes no network requests after it
has loaded, and nothing typed, sent or received leaves the device except as
sound.

## Building

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked
./build.sh
```

That compiles the engine to WebAssembly into `web/pkg/`. Serve `web/` and
open it:

```bash
python3 -m http.server --directory web 8000
```

Browsers only give a page the microphone over HTTPS or on `localhost`, so to
try it on a phone it has to be hosted.

## Testing

```bash
cargo test --release
```

## Publishing

Pushing to `main` runs `.github/workflows/pages.yml`, which tests the
engine, builds the site and deploys `web/` to GitHub Pages. In the
repository's settings, Pages must have its source set to "GitHub Actions".
