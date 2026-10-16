// /tv (src/web/tv.rs): a television set playing the channel's schedule
// (src/tv.rs). The set is a render (assets/blender/tv.py) with its tube's
// face cut out; behind the face, two of the video page's players stacked in
// one place, one on air and the other loading the next show out of sight
// (inert and hidden), seeked to where it starts, so the switch is a swap
// with no black frames between. A show airs for its slot's airtime, from its
// slot's starting point (a long video airs a stretch); a photo post turns
// its pictures over that time; a station break airs a bumper, and an "up
// next" card gets the next show's name written on it. Each show's sound
// goes through Web Audio at the gain that brings it to the TV's level
// (src/loudness.rs). A show that fails or stalls is skipped.
//
// The picture itself is drawn by the tube (assets/crt.js) from whatever is
// on air, with the set's on-screen display (channel, volume, mute) keyed
// over it. The set works as one did: its POWER button turns it on (the
// degaussing coil's wobble) and off (the picture collapsing to a dot), its
// channel buttons tune (a burst of snow and the picture rolling as it
// locks on), its knob sets the volume, and its LCD shows the channel. Where
// WebGL2 isn't available, the players show as they are, under CSS snow.
//
// Every switch is counted in `window.tvStats`, which a test run reads.

import { setUp } from "/assets/player.js";
import { Tube, RASTER } from "/assets/crt.js";
import { SetSound } from "/assets/tvsound.js";

// How long a show's picture may stand still while it should play before
// it's skipped, and how long a tuned-in show may take to start.
const STALL_MS = 10000;
const START_MS = 6000;
// Ask for more of the schedule when fewer shows than this are waiting.
const LOW_WATER = 2;
// A channel change shows snow for at least this long: the tuner's lock.
// Flipping fast (a press within FLIPPING_MS of the last), the snow holds
// until the remote rests for SETTLE_MS, so quick flipping is one stretch of
// snow rather than snow and picture in turn, which would flash.
const TUNE_MS = 220;
const FLIPPING_MS = 1000;
const SETTLE_MS = 600;
// How long the OSD shows the channel and the volume.
const OSD_CHANNEL_MS = 3500;
const OSD_VOLUME_MS = 2000;
// Stations never cut straight from one spot to the next: the picture fades
// to black, holds there a moment, and the next fades up (measured in
// off-air recordings of the day: 0.13-0.27 s out, 0.2-0.67 s of black,
// 0.1-0.27 s up). In seconds.
const FADE_OUT = 0.2;
const BLACK = 0.25;
const FADE_UP = 0.2;

const tv = document.getElementById("tv");
const screen = document.getElementById("tv-screen");
const stages = [...screen.querySelectorAll(".tv-stage")];
const canvas = screen.querySelector(".tv-crt");
const wake = screen.querySelector(".tv-wake");
const off = screen.querySelector(".tv-off");
const bug = screen.querySelector(".tv-bug");
const nowLine = tv.querySelector(".tv-now");
const nextLine = tv.querySelector(".tv-next");
const footer = document.getElementById("tv-footer");
const channels = [...tv.querySelectorAll(".tv-channel")];
const powerKey = tv.querySelector(".tv-power-key");
const knob = tv.querySelector(".tv-knob");
const lcd = tv.querySelector(".tv-lcd-lit");
const reduced = matchMedia("(prefers-reduced-motion: reduce)");

window.tvStats = { switches: [], skips: [], tunes: [], powered: () => powered, quality: () => quality };

let channel = tv.dataset.channel;
// Bumped by every tune, so answers for a channel left behind are dropped.
let tuning = 0;
let queue = [];
let fetching = null;
let onAir = null;
let next = null;
// The show a tune is getting ready, until it goes on air.
let joining = null;
let powered = false;

let sound = { volume: 0.8, muted: false };
try {
  sound = { ...sound, ...JSON.parse(localStorage.getItem("tv-sound") || "{}") };
} catch {
  // Storage blocked: the default level.
}

// Web Audio carries each show's gain, then a limiter that holds the peaks
// of a quiet show lifted to the TV's level at -1 dB, as broadcasters do
// (src/loudness.rs counts on it taking up to 3 dB). Browsers start it
// suspended until the page may play sound, which is also when the TV may
// turn itself on.
let audio = null;
let limiter = null;
let setSound = null;
try {
  audio = new AudioContext();
  limiter = new DynamicsCompressorNode(audio, { threshold: -1, knee: 0, ratio: 20, attack: 0.002, release: 0.15 });
  limiter.connect(audio.destination);
  setSound = new SetSound(audio, limiter);
  setSound.level(sound.volume, sound.muted);
} catch {
  // No Web Audio: shows play at their own levels, and the set is silent.
}

// The tube, unless the browser can't draw it: then the players show as
// they are.
let crt = null;
try {
  crt = new Tube(canvas);
} catch (e) {
  console.info("tv: drawing the picture plainly:", e.message);
  tv.classList.add("flat");
}

// ---- Shows ----------------------------------------------------------------

// Put `slot`'s player in `stage`, paused at `offset` seconds into its slot.
function prepare(slot, stage, offset = 0) {
  const abort = new AbortController();
  // Parsed in a detached element rather than a <template>: Chromium won't
  // play a media element made in a template's document, even where the
  // page may play sound.
  const content = document.createElement("div");
  content.innerHTML = slot.player;
  // The TV has its own keys, and a waiting player mustn't start its sound
  // or answer them.
  for (const el of content.querySelectorAll("media-hotkey, media-gesture[type=doubletap]")) el.remove();
  for (const el of content.querySelectorAll("audio[autoplay]")) el.removeAttribute("autoplay");
  for (const el of content.querySelectorAll(".player video:not(.slide-video), .player audio")) el.preload = "auto";
  stage.replaceChildren(...content.childNodes);
  setUp(stage, abort.signal);

  const show = {
    slot,
    stage,
    abort,
    media: stage.querySelector(".player video:not(.slide-video), .player audio"),
    pictures: [...stage.querySelectorAll(".slide-dot")],
    slides: [...stage.querySelectorAll(".slides > *")],
    start: slot.from + offset,
    // A photo post's time on air, counted while it plays.
    shown: offset,
    lastTick: null,
    failed: false,
    ready: false,
    stillSince: null,
    lastTime: -1,
  };
  const { signal } = abort;
  stage.addEventListener("player-failed", () => (show.failed = true), { signal });
  show.whenReady = new Promise((resolve) => {
    const ready = () => {
      show.ready = true;
      resolve();
    };
    const media = show.media;
    if (media) {
      media.volume = sound.volume;
      media.muted = sound.muted;
      show.route = level(media, slot.gain_db);
      // A photo post's sound loops from the top; a video starts where its
      // slot does.
      const seek = () => {
        if (media.localName === "video" && show.start > 0.5 && Math.abs(media.currentTime - show.start) > 0.5) {
          media.currentTime = show.start;
        }
      };
      media.addEventListener("loadedmetadata", seek, { signal });
      if (media.readyState >= 1) seek();
      const check = () => media.readyState >= 3 && !media.seeking && ready();
      media.addEventListener("canplay", check, { signal });
      media.addEventListener("seeked", check, { signal });
      check();
      media.addEventListener("ended", () => show === onAir && change("ended"), { signal });
    } else {
      const first = stage.querySelector(".slides > img");
      if (!first || first.complete) ready();
      else {
        first.addEventListener("load", ready, { once: true, signal });
        first.addEventListener("error", ready, { once: true, signal });
      }
    }
    stage.addEventListener("player-failed", ready, { signal });
  });
  return show;
}

// Route `media`'s sound through a gain of `db` decibels. Returns the nodes,
// which `tearDown` disconnects: while connected, they keep the media (and
// all it buffered) alive.
function level(media, db) {
  if (!audio) return [];
  try {
    const gain = audio.createGain();
    gain.gain.value = Math.pow(10, (db || 0) / 20);
    const source = audio.createMediaElementSource(media);
    source.connect(gain).connect(limiter);
    return [source, gain];
  } catch {
    // Already routed, or the browser won't: its own level, then.
    return [];
  }
}

function tearDown(show) {
  if (!show) return;
  show.abort.abort();
  for (const node of show.route ?? []) node.disconnect();
  for (const media of show.stage.querySelectorAll("video, audio")) {
    media.pause();
    media.removeAttribute("src");
    media.load();
  }
  show.stage.replaceChildren();
}

// How long `show` has been on air, in seconds of its slot.
function elapsed(show) {
  if (show.media?.localName === "video") return show.media.currentTime - show.slot.from;
  return show.shown;
}

function remaining(show) {
  let end = show.slot.airtime;
  // The file can be a little shorter than the length the platform gave.
  if (show.media?.localName === "video" && show.media.duration) {
    end = Math.min(end, show.media.duration - show.slot.from);
  }
  return end - elapsed(show);
}

// Put `show` on screen and, once the TV is on, play it.
function air(show) {
  const old = onAir;
  onAir = show;
  show.airedAt = performance.now();
  show.stage.removeAttribute("inert");
  show.stage.classList.add("on-air");
  for (const stage of stages) {
    if (stage !== show.stage) {
      stage.setAttribute("inert", "");
      stage.classList.remove("on-air");
    }
  }
  if (old && old !== show) tearDown(old);
  if (powered) play(show);
  else tv.classList.remove("tuning");
  guide();
  ahead();
}

function play(show) {
  const media = show.media;
  const started = performance.now();
  const record = () => {
    const ms = Math.round(performance.now() - (show.cueAt ?? started));
    // From the channel button to the picture, or from one show to the next.
    if (show.tuned) window.tvStats.tunes.push({ channel, ms });
    else window.tvStats.switches.push({ seq: show.slot.seq, title: show.slot.title, gapMs: ms });
    locked();
  };
  if (!media) {
    record();
    return;
  }
  media.addEventListener("playing", record, { once: true, signal: show.abort.signal });
  media.play().catch((e) => {
    // Held back by the browser's autoplay rules: wait for the power button.
    if (e.name === "NotAllowedError") switchOff(false);
  });
}

// Fill the other stage with the next show in the queue.
function ahead() {
  if (next || !queue.length) {
    refill();
    return;
  }
  const stage = stages.find((s) => s !== onAir?.stage);
  next = prepare(queue.shift(), stage);
  refill();
}

// Take the show off and put on the next one, ready or not: `why` it went.
function change(why) {
  if (!onAir) return;
  // Counted once, though the clock keeps asking while no show is ready.
  if (why !== "ended" && why !== "over" && !onAir.skipped) {
    onAir.skipped = true;
    window.tvStats.skips.push({ seq: onAir.slot.seq, title: onAir.slot.title, why });
    console.debug(`tv: skipped ${onAir.slot.title} (${why})`);
  }
  if (!next) {
    // The schedule hasn't come yet: hold the last show's picture until it does.
    ahead();
    if (!next) return;
  }
  const show = next;
  next = null;
  show.cueAt = performance.now();
  air(show);
}

// ---- The clock --------------------------------------------------------------

let lastTick = performance.now();
setInterval(() => {
  const now = performance.now();
  const step = (now - lastTick) / 1000;
  lastTick = now;
  if (!onAir || !powered) return;
  const show = onAir;
  const media = show.media;
  const playing = media ? !media.paused : true;

  if (show.failed) return change("failed");
  if (show.media?.localName !== "video" && playing) show.shown += step;

  // A photo post turns its pictures evenly over its airtime.
  if (show.pictures.length > 1) {
    const each = show.slot.airtime / show.pictures.length;
    const at = Math.min(show.pictures.length - 1, Math.floor(elapsed(show) / each));
    const dot = show.pictures[at];
    if (!dot.hasAttribute("aria-current")) dot.click();
  }

  if (remaining(show) <= 0.05) return change("over");

  // Playing, but the picture stands still: stalled.
  if (media && playing) {
    if (media.currentTime !== show.lastTime) {
      show.lastTime = media.currentTime;
      show.stillSince = now;
    } else if (now - (show.stillSince ?? now) > (media.currentTime > 0 ? STALL_MS : START_MS)) {
      return change("stalled");
    }
  } else {
    show.stillSince = now;
  }

  // The show waiting failed before its turn: wait for the one after.
  if (next?.failed) {
    window.tvStats.skips.push({ seq: next.slot.seq, title: next.slot.title, why: "failed early" });
    tearDown(next);
    next = null;
    ahead();
  }
  guide();
}, 100);

// ---- The schedule -----------------------------------------------------------

async function lineup(from) {
  const url = `/tv/${channel}/lineup` + (from == null ? "" : `?from=${from}`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status}`);
  return response.json();
}

// Ask for more of the schedule when it runs low.
function refill() {
  const waiting = queue.length + (next ? 1 : 0);
  if (fetching || waiting >= LOW_WATER) return;
  const last = queue.at(-1) ?? next?.slot ?? onAir?.slot;
  if (!last) return;
  const ticket = tuning;
  fetching = lineup(last.seq + 1)
    .then((answer) => {
      if (ticket !== tuning) return;
      const known = new Set([...queue.map((s) => s.seq), next?.slot.seq, onAir?.slot.seq]);
      queue.push(...answer.slots.filter((s) => !known.has(s.seq)));
      if (!next) ahead();
    })
    .catch((e) => console.warn("tv: couldn't get the schedule", e))
    .finally(() => {
      fetching = null;
    });
}

// Switch to channel `to` and join whatever is on.
let lastTune = -1e9;
async function tune(to) {
  const ticket = ++tuning;
  const asked = performance.now();
  const hold = asked - lastTune < FLIPPING_MS ? SETTLE_MS : TUNE_MS;
  lastTune = asked;
  channel = to;
  tv.dataset.channel = to;
  tv.classList.add("tuning");
  off.hidden = true;
  const button = channels.find((a) => a.dataset.channel === to);
  for (const a of channels) a === button ? a.setAttribute("aria-current", "page") : a.removeAttribute("aria-current");
  const number = channels.indexOf(button) + 1;
  const name = button.lastChild.textContent;
  bug.querySelector("b").textContent = number;
  bug.querySelector("span").textContent = name;
  bug.classList.remove("shown");
  void bug.offsetWidth;
  bug.classList.add("shown");
  osd.channel = { number, name, until: performance.now() + OSD_CHANNEL_MS };
  osd.dirty = true;
  if (lcd) lcd.textContent = String(number).padStart(2, "0");
  if (footer) footer.textContent = `Channel ${number} · ${name}`;
  const steps = tv.querySelectorAll(".tv-step");
  steps[0].href = channels.at(number - 2 < 0 ? -1 : number - 2).href;
  steps[1].href = channels[number % channels.length].href;
  if (powered) {
    setSound?.clunk();
    setSound?.hiss(true);
  }

  tearDown(onAir);
  tearDown(next);
  tearDown(joining);
  onAir = next = joining = null;
  queue = [];
  nowLine.textContent = nextLine.textContent = "…";

  let answer;
  try {
    answer = await lineup(null);
  } catch (e) {
    if (ticket !== tuning) return;
    off.textContent = "Can't reach Joyscroller. Is it still on?";
    off.hidden = false;
    deadAir();
    return;
  }
  if (ticket !== tuning) return;
  if (!answer.slots.length) {
    off.textContent = `Nothing on ${name} yet: its posts air here once they're downloaded.`;
    off.hidden = false;
    deadAir();
    guide();
    return;
  }
  queue = answer.slots;
  const first = queue.shift();
  // The channel has been on all along: join the show partway through.
  const offset = Math.max(0, answer.now - first.starts_at + (performance.now() - asked) / 1000);
  const show = prepare(first, stages[0], offset);
  joining = show;
  const lock = new Promise((r) => setTimeout(r, hold - (performance.now() - asked)));
  await Promise.race([Promise.all([show.whenReady, lock]), new Promise((r) => setTimeout(r, START_MS))]);
  if (ticket !== tuning) return;
  joining = null;
  show.cueAt = asked;
  show.tuned = true;
  air(show);
}

// Nothing to tune in: the snow and hiss stop, leaving a dark screen for
// the message over it.
function deadAir() {
  tv.classList.remove("tuning");
  setSound?.hiss(false);
}

// The tuner has locked on: the snow and hiss stop, and the picture rolls
// into place.
function locked() {
  if (!tv.classList.contains("tuning")) return;
  tv.classList.remove("tuning");
  setSound?.hiss(false);
  set.rollFrom = performance.now();
}

// ---- The set ------------------------------------------------------------------

// What the set is doing, for the tube: when it last came on, went off, or
// locked on to a channel.
const set = { onAt: -1e9, offAt: -1e9, rollFrom: -1e9 };

function switchOn() {
  if (powered) return;
  powered = true;
  tv.classList.remove("off");
  wake.hidden = true;
  powerKey?.setAttribute("aria-pressed", "true");
  set.onAt = performance.now();
  set.offAt = -1e9;
  audio?.resume().catch(() => {});
  setSound?.powerOn();
  // Time has gone by: join what's on now.
  tune(channel);
}

// Off: the picture collapses (`animate`, when the viewer turned it off),
// and the shows stop. Turning it on again joins what's on then.
function switchOff(animate = true) {
  const was = powered;
  powered = false;
  tv.classList.add("off");
  tv.classList.remove("tuning");
  wake.hidden = false;
  powerKey?.setAttribute("aria-pressed", "false");
  if (was && animate) {
    set.offAt = performance.now();
    setSound?.powerOff();
  } else {
    set.offAt = -1e9;
  }
  setSound?.hiss(false);
  ++tuning;
  tearDown(onAir);
  tearDown(next);
  tearDown(joining);
  onAir = next = joining = null;
  queue = [];
  nowLine.textContent = nextLine.textContent = "…";
}

powerKey?.addEventListener("click", () => (powered ? switchOff() : switchOn()));
wake.addEventListener("click", switchOn);

function setVolume(volume, muted = sound.muted) {
  sound = { volume: Math.min(1, Math.max(0, Math.round(volume * 100) / 100)), muted };
  for (const show of [onAir, next, joining]) if (show?.media) Object.assign(show.media, sound);
  setSound?.level(sound.volume, sound.muted);
  showKnob();
  try {
    localStorage.setItem("tv-sound", JSON.stringify(sound));
  } catch {
    // Storage blocked: remembered for this visit only.
  }
}

function showKnob() {
  if (!knob) return;
  const frames = Number(knob.dataset.frames);
  const at = Math.round(sound.volume * (frames - 1));
  knob.style.backgroundPositionY = `${(at / (frames - 1)) * 100}%`;
  knob.setAttribute("aria-valuenow", String(Math.round(sound.volume * 100)));
  knob.setAttribute("aria-valuetext", sound.muted ? "muted" : `${Math.round(sound.volume * 100)}%`);
}

function nudge(by) {
  setVolume(sound.volume + by, false);
  osd.volume = performance.now() + OSD_VOLUME_MS;
  osd.dirty = true;
}

if (knob) {
  showKnob();
  knob.addEventListener("wheel", (e) => {
    e.preventDefault();
    nudge(e.deltaY < 0 ? 0.04 : -0.04);
  }, { passive: false });
  // Dragging turns it: up or right is louder.
  let drag = null;
  knob.addEventListener("pointerdown", (e) => {
    drag = { x: e.clientX, y: e.clientY, from: sound.volume };
    knob.setPointerCapture(e.pointerId);
  });
  knob.addEventListener("pointermove", (e) => {
    if (!drag) return;
    const by = (e.clientX - drag.x - (e.clientY - drag.y)) / 160;
    setVolume(drag.from + by, false);
    osd.volume = performance.now() + OSD_VOLUME_MS;
    osd.dirty = true;
  });
  knob.addEventListener("pointerup", () => (drag = null));
  knob.addEventListener("pointercancel", () => (drag = null));
  knob.addEventListener("keydown", (e) => {
    const by = { ArrowUp: 0.05, ArrowRight: 0.05, ArrowDown: -0.05, ArrowLeft: -0.05, PageUp: 0.2, PageDown: -0.2 }[e.key];
    if (by == null) return;
    e.preventDefault();
    e.stopPropagation();
    nudge(by);
  });
}

// ---- Keys and the remote --------------------------------------------------------

function switchTo(to) {
  if (to === channel && onAir) return;
  const button = channels.find((a) => a.dataset.channel === to);
  history.replaceState(null, "", button.href);
  if (powered) tune(to);
  else {
    channel = to;
    switchOn();
  }
}

function step(by) {
  const at = channels.findIndex((a) => a.dataset.channel === channel);
  const to = channels[(at + by + channels.length) % channels.length];
  switchTo(to.dataset.channel);
}

for (const a of channels) {
  a.addEventListener("click", (e) => {
    e.preventDefault();
    switchTo(a.dataset.channel);
  });
}
for (const a of tv.querySelectorAll(".tv-step, .tv-ch-down, .tv-ch-up")) {
  a.addEventListener("click", (e) => {
    e.preventDefault();
    step(Number(a.dataset.step));
  });
}

addEventListener("keydown", (e) => {
  if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey) return;
  if (e.target.closest?.("input, textarea, [role=slider], dialog")) return;
  // Space on a focused button presses it.
  if (e.key === " " && e.target.closest?.("button, a")) return;
  const media = onAir?.media;
  const key = e.key;
  if (key === "ArrowUp" || key === "PageUp") step(1);
  else if (key === "ArrowDown" || key === "PageDown") step(-1);
  else if (/^[1-9]$/.test(key) && channels[key - 1]) switchTo(channels[key - 1].dataset.channel);
  else if (key === "m") {
    setVolume(sound.volume, !sound.muted);
    osd.dirty = true;
  } else if (key === "+" || key === "=") nudge(0.05);
  else if (key === "-" || key === "_") nudge(-0.05);
  else if (key === "p") powered ? switchOff() : switchOn();
  else if ((key === " " || key === "k") && media && powered) media.paused ? media.play() : media.pause();
  else if (key === "f") fullscreen();
  else return;
  e.preventDefault();
});

function fullscreen() {
  if (document.fullscreenElement) document.exitFullscreen();
  else screen.requestFullscreen?.();
}
// The players' own fullscreen button would fill the screen with one show
// only; the TV fills it with both, so the next show takes over in place.
screen.addEventListener(
  "click",
  (e) => {
    if (!e.target.closest?.("media-fullscreen-button")) return;
    e.preventDefault();
    e.stopPropagation();
    fullscreen();
  },
  true,
);
screen.addEventListener("dblclick", (e) => {
  if (!e.target.closest?.(".player-strip, .slide-turn, .slide-dots, .tv-wake")) fullscreen();
});

// The viewer's volume, from a player's own controls (the flat set shows
// them), carries from show to show.
screen.addEventListener(
  "volumechange",
  (e) => {
    if (e.target !== onAir?.media) return;
    if (e.target.volume === sound.volume && e.target.muted === sound.muted) return;
    setVolume(e.target.volume, e.target.muted);
  },
  true,
);

// ---- The guide --------------------------------------------------------------

function describe(slot, link) {
  const title = link && slot.href ? `<a href="${slot.href}">${escape(slot.title)}</a>` : escape(slot.title);
  const who = slot.author ? ` · ${escape(slot.author)}` : "";
  return `${title}<span class="tv-who">${who} · ${escape(slot.platform)}</span>`;
}

let described = null;
function guide() {
  const upNext = next?.slot ?? queue[0];
  const key = `${onAir?.slot.seq}/${upNext?.seq}`;
  if (key !== described) {
    described = key;
    nowLine.innerHTML = onAir ? describe(onAir.slot, true) + ' <span class="tv-left"></span>' : "…";
    nextLine.innerHTML = upNext ? describe(upNext, false) : "…";
  }
  const left = nowLine.querySelector(".tv-left");
  if (left && onAir) left.textContent = clock(remaining(onAir)) + " left";
}

function clock(seconds) {
  const s = Math.max(0, Math.round(seconds));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function escape(text) {
  const el = document.createElement("span");
  el.textContent = text;
  return el.innerHTML;
}

// ---- The picture ----------------------------------------------------------------

// The set's on-screen display, drawn into the raster after decoding, in a
// 5 x 7 dot font, green with a black shadow, as sets of the day drew it:
// the channel in the top right after tuning, the volume as a bar along the
// bottom, MUTE in the top left.
const GLYPHS = {
  0: "01110100011001110101110011000101110", 1: "00100011000010000100001000010001110",
  2: "01110100010000100010001000100011111", 3: "11111000100010000010000011000101110",
  4: "00010001100101010010111110001000010", 5: "11111100001111000001000011000101110",
  6: "00110010001000011110100011000101110", 7: "11111000010001000100010000100001000",
  8: "01110100011000101110100011000101110", 9: "01110100011000101111000010001001100",
  A: "01110100011000111111100011000110001", B: "11110100011000111110100011000111110",
  C: "01110100011000010000100001000101110", D: "11100100101000110001100011001011100",
  E: "11111100001000011110100001000011111", F: "11111100001000011110100001000010000",
  G: "01110100011000010111100011000101111", H: "10001100011000111111100011000110001",
  I: "01110001000010000100001000010001110", J: "00111000100001000010000101001001100",
  K: "10001100101010011000101001001010001", L: "10000100001000010000100001000011111",
  M: "10001110111010110101100011000110001", N: "10001100011100110101100111000110001",
  O: "01110100011000110001100011000101110", P: "11110100011000111110100001000010000",
  Q: "01110100011000110001101011001001101", R: "11110100011000111110101001001010001",
  S: "01111100001000001110000010000111110", T: "11111001000010000100001000010000100",
  U: "10001100011000110001100011000101110", V: "10001100011000110001100010101000100",
  W: "10001100011000110101101011010101010", X: "10001100010101000100010101000110001",
  Y: "10001100011000101010001000010000100", Z: "11111000010001000100010001000011111",
  "-": "00000000000000011111000000000000000", " ": "00000000000000000000000000000000000",
};

const osd = { channel: null, volume: 0, dirty: true, canvas: null, shown: false };
const graphics = { canvas: null, key: null };

function layer() {
  const c = document.createElement("canvas");
  c.width = RASTER.width;
  c.height = RASTER.height;
  return c;
}

function dots(ctx, text, x, y, scale, align = "left") {
  const width = text.length * 6 * scale - scale;
  let at = align === "right" ? x - width : align === "center" ? x - width / 2 : x;
  for (const ch of text.toUpperCase()) {
    const g = GLYPHS[ch] ?? GLYPHS[" "];
    for (let i = 0; i < 35; i++) {
      if (g[i] === "1") ctx.fillRect(at + (i % 5) * scale, y + Math.floor(i / 5) * scale, scale, scale);
    }
    at += 6 * scale;
  }
}

function drawOsd(now) {
  osd.canvas ??= layer();
  const ctx = osd.canvas.getContext("2d");
  ctx.clearRect(0, 0, RASTER.width, RASTER.height);
  let any = false;
  const both = (draw) => {
    ctx.fillStyle = "rgba(0, 0, 0, 0.85)";
    ctx.save();
    ctx.translate(3, 3);
    draw();
    ctx.restore();
    ctx.fillStyle = "#3dff5a";
    draw();
  };
  // Inside the 5% the set's overscan hides, and then some.
  if (osd.channel && (now < osd.channel.until || tv.classList.contains("tuning"))) {
    any = true;
    both(() => {
      dots(ctx, String(osd.channel.number), 596, 40, 9, "right");
      dots(ctx, osd.channel.name, 596, 112, 3, "right");
    });
  }
  if (now < osd.volume) {
    any = true;
    both(() => {
      dots(ctx, "VOLUME", 60, 392, 3);
      const lit = Math.round(sound.volume * 30);
      for (let k = 0; k < 30; k++) {
        const h = k < lit ? 22 : 4;
        ctx.fillRect(60 + k * 17, 438 - h, 11, h);
      }
    });
  }
  if (sound.muted) {
    any = true;
    both(() => dots(ctx, "MUTE", 48, 40, 4));
  }
  osd.shown = any;
  return any;
}

// An "up next" card's text: the next show's name, set on the bumper's blank
// panel (its box and times come from bumpers.json), popping on as the
// bumper's own drawings do, on twos.
function drawGraphics(show) {
  const card = show?.slot.bumper?.text;
  // The server names the show after the break; if it couldn't (that show
  // was dropped), the one the page has waiting is what's next.
  const waiting = next?.slot ?? queue[0];
  const upNext = show?.slot.next ?? (waiting && !waiting.bumper ? waiting : null);
  const media = show?.media;
  if (!card || !upNext || !media) return null;
  const t = media.currentTime;
  if (t < card.from || t > card.to) return null;
  const drawing = Math.floor((t - card.from) * 12);
  const pop = [0.4, 1.12, 0.96, 1][Math.min(3, drawing)];
  const key = `${show.slot.seq}/${pop}`;
  graphics.canvas ??= layer();
  if (graphics.key === key) return graphics.canvas;
  graphics.key = key;
  const ctx = graphics.canvas.getContext("2d");
  ctx.clearRect(0, 0, RASTER.width, RASTER.height);
  const [x, y, w, h] = card.box;
  ctx.save();
  ctx.translate(x + w / 2, y + h / 2);
  ctx.scale(pop, pop);
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  // The title in the bumper's lettering, as big as fits on two lines, and
  // no smaller than reads across a room: past that, cut short.
  const title = cardTitle(upNext);
  let size = 40;
  let lines;
  for (; size >= 26; size -= 2) {
    ctx.font = `${size}px "Luckiest Guy", "Fredoka", sans-serif`;
    lines = wrap(ctx, title, w - 24);
    if (lines.length <= 2) break;
  }
  size = Math.max(size, 26);
  if (lines.length > 2) lines = [lines[0], ellipsize(ctx, lines.slice(1).join(" "), w - 24)];
  const who = [upNext.author, upNext.platform].filter(Boolean).join(" · ");
  const lineH = size * 1.05;
  const top = -((lines.length - 1) * lineH) / 2 - (who ? 11 : 0);
  // White on the card's purple panel with a hard black shadow, as the
  // day's endcards set it.
  ctx.lineJoin = "round";
  lines.forEach((line, k) => {
    ctx.fillStyle = "#000";
    ctx.fillText(line, 4, top + k * lineH + 4);
    ctx.lineWidth = 3;
    ctx.strokeStyle = "#1d0f3f";
    ctx.strokeText(line, 0, top + k * lineH);
    ctx.fillStyle = "#fbfbf4";
    ctx.fillText(line, 0, top + k * lineH);
  });
  if (who) {
    ctx.font = `600 19px "Fredoka", sans-serif`;
    const at = top + lines.length * lineH + 4;
    ctx.fillStyle = "#000";
    ctx.fillText(ellipsize(ctx, who, w - 24), 2, at + 2);
    ctx.fillStyle = "#b6f23a";
    ctx.fillText(ellipsize(ctx, who, w - 24), 0, at);
  }
  ctx.restore();
  return graphics.canvas;
}

// What a card calls a show: its caption without the hashtags, which
// stations never read out (a TikTok's can be nothing but), or failing that
// its creator.
function cardTitle(slot) {
  const plain = (slot.title || "").replace(/#[^\s#]+/g, "").replace(/\s+/g, " ").trim();
  return /[\p{L}\p{N}]/u.test(plain) ? plain : slot.author || "Something good";
}

// `text` in lines no wider than `width`; a word too long for a line is
// broken where it overflows.
function wrap(ctx, text, width) {
  const words = [];
  for (const word of text.split(/\s+/)) {
    let rest = word;
    while (rest && ctx.measureText(rest).width > width) {
      let cut = rest.length - 1;
      while (cut > 1 && ctx.measureText(rest.slice(0, cut)).width > width) cut--;
      words.push(rest.slice(0, cut));
      rest = rest.slice(cut);
    }
    if (rest) words.push(rest);
  }
  const lines = [];
  let line = "";
  for (const word of words) {
    const tryLine = line ? `${line} ${word}` : word;
    if (ctx.measureText(tryLine).width > width && line) {
      lines.push(line);
      line = word;
    } else line = tryLine;
  }
  if (line) lines.push(line);
  return lines;
}

function ellipsize(ctx, text, width) {
  if (ctx.measureText(text).width <= width) return text;
  let cut = text;
  while (cut.length > 1 && ctx.measureText(cut + "…").width > width) cut = cut.slice(0, -1);
  return cut.trimEnd() + "…";
}

// What `show` has on screen now: its video, the photo post's picture (or
// carousel video) on show, or nothing yet.
function pictureOf(show) {
  if (!show) return null;
  if (show.slides.length) {
    const at = show.pictures.findIndex((d) => d.hasAttribute("aria-current"));
    return show.slides[Math.max(0, at)] ?? null;
  }
  return show.media?.localName === "video" ? show.media : null;
}

// Fit the canvas to the face it covers, in device pixels: no more than 1440
// tall (three pixels a scanline, past which there's nothing more to draw),
// and less while the GPU can't keep up (`quality`, which the frame loop
// turns down when frames run late and back up when there's room).
let quality = 1;
function resize() {
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const scale = Math.min(quality, 1440 / Math.max(1, canvas.clientHeight * dpr));
  const w = Math.round(canvas.clientWidth * dpr * scale);
  const h = Math.round(canvas.clientHeight * dpr * scale);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
}

// Where the tube's face is in the canvas: all of it on the set; in
// fullscreen, the largest 4:3 that fits, as a tube that size would show it.
function face() {
  const { width: w, height: h } = canvas;
  if (document.fullscreenElement === screen) {
    const fh = Math.min(h, (w * 3) / 4);
    const fw = (fh * 4) / 3;
    return [(w - fw) / 2, (h - fh) / 2, fw, fh];
  }
  return [0, 0, w, h];
}

let drewOff = false;
// Frame pacing: the average interval between frames, the display's own
// interval (the shortest seen lately: 16.7 ms at 60 Hz, 33 ms on a 30 Hz
// display or in a browser's battery saver), and since when frames have run
// late against it, or had room.
const pace = { last: performance.now(), avg: 16.7, base: 16.7, lateSince: null, roomSince: null };
function frame() {
  requestAnimationFrame(frame);
  if (!crt) return;
  const now = performance.now();
  const interval = now - pace.last;
  pace.last = now;
  if (interval < 100) {
    pace.avg += (interval - pace.avg) * 0.1;
    pace.base = interval < pace.base ? interval : pace.base + (interval - pace.base) * 0.002;
    if (pace.avg > pace.base * 1.3) {
      pace.lateSince ??= now;
      pace.roomSince = null;
      if (now - pace.lateSince > 1000 && quality > 0.5) {
        quality = Math.max(0.5, quality * 0.8);
        pace.lateSince = null;
      }
    } else if (pace.avg < pace.base * 1.08 && quality < 1) {
      pace.roomSince ??= now;
      pace.lateSince = null;
      if (now - pace.roomSince > 5000) {
        quality = Math.min(1, quality * 1.15);
        pace.roomSince = null;
      }
    } else {
      pace.lateSince = pace.roomSince = null;
    }
  }
  const calm = reduced.matches;
  const sinceOff = (now - set.offAt) / 1000;
  const sinceOn = (now - set.onAt) / 1000;
  const collapsing = !powered && sinceOff < 2;
  // Off and done collapsing: the dark glass needs drawing only once.
  if (!powered && !collapsing) {
    if (drewOff) return;
    drewOff = true;
  } else drewOff = false;
  resize();
  const box = face();
  const tuningNow = powered && tv.classList.contains("tuning");
  const show = tuningNow ? null : onAir;
  if (powered && !tuningNow) crt.source(pictureOf(show));
  if (powered) crt.layer("graphics", drawGraphics(show));
  if (osd.dirty || osd.shown) {
    osd.dirty = false;
    crt.layer("osd", powered && drawOsd(now) ? osd.canvas : null);
  }
  const sinceLock = (now - set.rollFrom) / 1000;
  // Between spots: fading out at a show's end, and black then fading up at
  // the next's start (not a show joined by tuning in, which is mid-way).
  let spot = 1;
  if (show && powered) {
    spot = Math.min(1, Math.max(0, remaining(show) / FADE_OUT));
    if (!show.tuned) spot = Math.min(spot, Math.max(0, ((now - show.airedAt) / 1000 - BLACK) / FADE_UP));
  }
  crt.draw(
    {
      face: box,
      power: powered ? (calm ? 1 : Math.min(1, sinceOn / 0.5)) * spot : collapsing && !calm ? 1 : 0,
      collapse: collapsing && !calm ? Math.min(1, sinceOff / 1.8) : 0,
      degauss: powered && !calm ? Math.max(0, 1 - sinceOn / 1.6) : 0,
      snow: tuningNow ? 1 : 0,
      // The picture rolls down into place as the set locks on.
      // Not far: the blanking bar slides off the top rather than sweeping
      // through the picture, which would flash.
      roll: !calm && sinceLock < 0.6 ? -0.12 * Math.exp(-sinceLock * 7) * (1 - sinceLock / 0.6) : 0,
      jitter: !calm && sinceLock < 0.4 ? 6 * (1 - sinceLock / 0.4) : 0.4,
      noise: 0.025,
    },
    calm ? 0 : now / 1000,
  );
}

if (crt) {
  new ResizeObserver(() => (drewOff = false)).observe(canvas);
  requestAnimationFrame(frame);
  // The GPU reset (a driver update, memory pressure): show the players as
  // they are until the browser gives the context back, then the tube again.
  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    crt = null;
    tv.classList.add("flat");
  });
  canvas.addEventListener("webglcontextrestored", () => {
    try {
      crt = new Tube(canvas);
      tv.classList.remove("flat");
      osd.dirty = true;
      drewOff = false;
    } catch (e) {
      console.info("tv: drawing the picture plainly:", e.message);
    }
  });
}

// ---- Turning on -------------------------------------------------------------

// Browsers let a page play sound once the viewer has interacted with the
// site; otherwise the set waits, off, for its POWER button. Web Audio says
// which: it starts suspended, and resumes by itself only where sound is
// allowed.
if (lcd) lcd.textContent = String(channels.findIndex((a) => a.dataset.channel === channel) + 1).padStart(2, "0");
(async () => {
  if (audio) await Promise.race([audio.resume().catch(() => {}), new Promise((r) => setTimeout(r, 500))]);
  if (audio ? audio.state === "running" : navigator.userActivation?.hasBeenActive) switchOn();
  else switchOff(false);
})();
