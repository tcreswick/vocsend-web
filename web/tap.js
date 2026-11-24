// Runs on the audio thread: passes each block of microphone samples to the page.
registerProcessor("tap", class extends AudioWorkletProcessor {
  process(inputs) {
    const channel = inputs[0][0];
    if (channel) this.port.postMessage(channel.slice());
    return true;
  }
});
