// AudioWorkletProcessor around the Rust mixer compiled to a plain wasm module (no imports, so no glue needed
// in the worklet scope). The main thread passes the module bytes in through processorOptions.
class MixerProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { bytes, rate, secs, fadeAt, fade } = options.processorOptions;
    this.x = new WebAssembly.Instance(new WebAssembly.Module(bytes), {}).exports;
    this.e = this.x.engine_new(rate, secs, fadeAt, fade);
    this.frames = 0;
    this.quanta = 0;
    this.port.onmessage = () => this.port.postMessage({ frames: this.frames, quanta: this.quanta, time: currentTime });
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const n = this.x.engine_render(this.e);
    // memory.buffer is re-read every call: it is replaced if the wasm memory ever grows.
    const mem = this.x.memory.buffer;
    out[0].set(new Float32Array(mem, this.x.engine_left(this.e), 128));
    if (out[1]) out[1].set(new Float32Array(mem, this.x.engine_right(this.e), 128));
    this.frames += n;
    this.quanta += 1;
    return true;
  }
}
registerProcessor('mixer', MixerProcessor);
