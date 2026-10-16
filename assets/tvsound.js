// /tv's set's own sounds (tv.js plays them), synthesized in Web Audio
// rather than recorded: the hiss of an empty channel, the clunk of the
// tuner changing channel, the thump and hum of the degaussing coil when the
// set comes on, and the tick and crackle of the tube discharging when it
// goes off. They play at the set's volume, through the same limiter as the
// shows, and at levels under the shows' (-16 LUFS): the hiss sits about 8
// dB below them, the clicks peak about where speech does.

export class SetSound {
  constructor(ctx, destination) {
    this.ctx = ctx;
    this.out = ctx.createGain();
    this.out.connect(destination);
    // Two seconds of white noise, looped or sliced for every sound here.
    const n = ctx.sampleRate * 2;
    this.noise = ctx.createBuffer(1, n, ctx.sampleRate);
    const data = this.noise.getChannelData(0);
    for (let i = 0; i < n; i++) data[i] = Math.random() * 2 - 1;
    this.hissing = null;
  }

  // The set's volume knob and mute.
  level(volume, muted) {
    this.out.gain.setTargetAtTime(muted ? 0 : volume, this.ctx.currentTime, 0.02);
  }

  burst(at, seconds, { gain = 0.5, low = 200, high = 8000, decay = 30 } = {}) {
    const src = new AudioBufferSourceNode(this.ctx, { buffer: this.noise });
    const hp = new BiquadFilterNode(this.ctx, { type: "highpass", frequency: low });
    const lp = new BiquadFilterNode(this.ctx, { type: "lowpass", frequency: high });
    const g = new GainNode(this.ctx, { gain: 0 });
    g.gain.setValueAtTime(gain, at);
    g.gain.setTargetAtTime(0, at, 1 / decay);
    src.connect(hp).connect(lp).connect(g).connect(this.out);
    src.start(at, Math.random() * 1.5, seconds);
  }

  thump(at, from, to, seconds, gain) {
    const osc = new OscillatorNode(this.ctx, { frequency: from });
    osc.frequency.setValueAtTime(from, at);
    osc.frequency.exponentialRampToValueAtTime(to, at + seconds);
    const g = new GainNode(this.ctx, { gain: 0 });
    g.gain.setValueAtTime(0, at);
    g.gain.linearRampToValueAtTime(gain, at + 0.004);
    g.gain.exponentialRampToValueAtTime(0.0001, at + seconds);
    osc.connect(g).connect(this.out);
    osc.start(at);
    osc.stop(at + seconds + 0.05);
  }

  // An empty channel's hiss, on until turned off: noise shaped like a
  // speaker's, cut off above the set's audio bandwidth.
  hiss(on) {
    const now = this.ctx.currentTime;
    if (on && !this.hissing) {
      const src = new AudioBufferSourceNode(this.ctx, { buffer: this.noise, loop: true });
      const hp = new BiquadFilterNode(this.ctx, { type: "highpass", frequency: 250 });
      const lp = new BiquadFilterNode(this.ctx, { type: "lowpass", frequency: 6500 });
      const g = new GainNode(this.ctx, { gain: 0 });
      g.gain.setTargetAtTime(0.09, now, 0.01);
      src.connect(hp).connect(lp).connect(g).connect(this.out);
      src.start();
      this.hissing = { src, g };
    } else if (!on && this.hissing) {
      const { src, g } = this.hissing;
      g.gain.setTargetAtTime(0, now, 0.03);
      src.stop(now + 0.2);
      this.hissing = null;
    }
  }

  // The tuner's detent, and the relay muting the speaker while it locks.
  clunk() {
    const at = this.ctx.currentTime + 0.005;
    this.burst(at, 0.03, { gain: 0.5, low: 1500, high: 9000, decay: 120 });
    this.thump(at, 180, 70, 0.06, 0.35);
    this.burst(at + 0.045, 0.02, { gain: 0.25, low: 2500, high: 10000, decay: 160 });
  }

  // Power on: the switch, then the degaussing coil's thump and its 60 Hz
  // hum dying away over a second, as the picture comes up.
  powerOn() {
    const at = this.ctx.currentTime + 0.01;
    this.burst(at, 0.03, { gain: 0.45, low: 1200, high: 9000, decay: 140 });
    this.thump(at + 0.02, 95, 55, 0.35, 0.55);
    const hum = new OscillatorNode(this.ctx, { type: "sawtooth", frequency: 60 });
    const lp = new BiquadFilterNode(this.ctx, { type: "lowpass", frequency: 420 });
    const g = new GainNode(this.ctx, { gain: 0 });
    g.gain.setValueAtTime(0, at);
    g.gain.linearRampToValueAtTime(0.16, at + 0.05);
    g.gain.exponentialRampToValueAtTime(0.0001, at + 1.3);
    hum.connect(lp).connect(g).connect(this.out);
    hum.start(at);
    hum.stop(at + 1.4);
    for (let k = 0; k < 6; k++) this.burst(at + 0.1 + Math.random() * 0.6, 0.01, { gain: 0.12, low: 3000, high: 12000, decay: 300 });
  }

  // Power off: the switch, a soft pop from the speaker, and the tube's
  // static discharge crackling for a moment.
  powerOff() {
    const at = this.ctx.currentTime + 0.01;
    this.hiss(false);
    this.burst(at, 0.03, { gain: 0.45, low: 1200, high: 9000, decay: 140 });
    this.thump(at + 0.01, 140, 40, 0.12, 0.35);
    for (let k = 0; k < 9; k++) this.burst(at + 0.05 + Math.random() * 0.9, 0.008, { gain: 0.1, low: 4000, high: 14000, decay: 400 });
  }
}
