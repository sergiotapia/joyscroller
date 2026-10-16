// /tv (src/web/tv.rs): plays the channel's schedule (src/tv.rs) on two of
// the video page's players stacked in one place. One is on air; the other
// loads the next show out of sight (inert and hidden), seeked to where it
// starts, so the switch is a swap with no black frames between. A show
// airs for its slot's airtime, from its slot's starting point (a long
// video airs a stretch); a photo post turns its pictures over that time.
// Each show's sound goes through Web Audio at the gain that brings it to
// the TV's level (src/loudness.rs). A show that fails or stalls is skipped.
//
// Every switch is counted in `window.tvStats`, which a test run reads.

import { setUp } from "/assets/player.js";

// How long a show's picture may stand still while it should play before
// it's skipped, and how long a tuned-in show may take to start.
const STALL_MS = 10000;
const START_MS = 6000;
// Ask for more of the schedule when fewer shows than this are waiting.
const LOW_WATER = 2;

const tv = document.getElementById("tv");
const screen = document.getElementById("tv-screen");
const stages = [...screen.querySelectorAll(".tv-stage")];
const power = screen.querySelector(".tv-power");
const off = screen.querySelector(".tv-off");
const bug = screen.querySelector(".tv-bug");
const nowLine = tv.querySelector(".tv-now");
const nextLine = tv.querySelector(".tv-next");
const footer = document.getElementById("tv-footer");
const channels = [...tv.querySelectorAll(".tv-channel")];

window.tvStats = { switches: [], skips: [], tunes: [], powered: () => powered };

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

let sound = { volume: 1, muted: false };
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
try {
  audio = new AudioContext();
  limiter = new DynamicsCompressorNode(audio, { threshold: -1, knee: 0, ratio: 20, attack: 0.002, release: 0.15 });
  limiter.connect(audio.destination);
} catch {
  // No Web Audio: shows play at their own levels.
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
    tv.classList.remove("tuning");
  };
  if (!media) {
    record();
    return;
  }
  media.addEventListener("playing", record, { once: true, signal: show.abort.signal });
  media.play().catch((e) => {
    // Held back by the browser's autoplay rules: wait for the power button.
    if (e.name === "NotAllowedError") turnedOff();
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
async function tune(to) {
  const ticket = ++tuning;
  const asked = performance.now();
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
  if (footer) footer.textContent = `Channel ${number} · ${name}`;
  const steps = tv.querySelectorAll(".tv-step");
  steps[0].href = channels.at(number - 2 < 0 ? -1 : number - 2).href;
  steps[1].href = channels[number % channels.length].href;

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
    return;
  }
  if (ticket !== tuning) return;
  if (!answer.slots.length) {
    off.textContent = `Nothing on ${name} yet: its posts air here once they're downloaded.`;
    off.hidden = false;
    tv.classList.remove("tuning");
    guide();
    return;
  }
  queue = answer.slots;
  const first = queue.shift();
  // The channel has been on all along: join the show partway through.
  const offset = Math.max(0, answer.now - first.starts_at + (performance.now() - asked) / 1000);
  const show = prepare(first, stages[0], offset);
  joining = show;
  await Promise.race([show.whenReady, new Promise((r) => setTimeout(r, START_MS))]);
  if (ticket !== tuning) return;
  joining = null;
  show.cueAt = asked;
  show.tuned = true;
  air(show);
}

// ---- Power, keys and the remote --------------------------------------------

function turnedOff() {
  powered = false;
  power.hidden = false;
  tv.classList.remove("tuning");
}

power.addEventListener("click", async () => {
  power.hidden = true;
  powered = true;
  await audio?.resume().catch(() => {});
  // Time has gone by since the page loaded: join what's on now.
  tune(channel);
});

function switchTo(to) {
  if (to === channel && onAir) return;
  const button = channels.find((a) => a.dataset.channel === to);
  history.replaceState(null, "", button.href);
  tune(to);
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
for (const a of tv.querySelectorAll(".tv-step")) {
  a.addEventListener("click", (e) => {
    e.preventDefault();
    step(Number(a.dataset.step));
  });
}

addEventListener("keydown", (e) => {
  if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey) return;
  if (e.target.closest?.("input, textarea, [role=slider], dialog")) return;
  const media = onAir?.media;
  const key = e.key;
  if (key === "ArrowUp" || key === "PageUp") step(1);
  else if (key === "ArrowDown" || key === "PageDown") step(-1);
  else if (/^[1-9]$/.test(key) && channels[key - 1]) switchTo(channels[key - 1].dataset.channel);
  else if (key === "m" && media) media.muted = !media.muted;
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
  if (!e.target.closest?.(".player-strip, .slide-turn, .slide-dots")) fullscreen();
});

// The viewer's volume carries from show to show.
screen.addEventListener(
  "volumechange",
  (e) => {
    if (e.target !== onAir?.media) return;
    sound = { volume: e.target.volume, muted: e.target.muted };
    if (next?.media) Object.assign(next.media, sound);
    try {
      localStorage.setItem("tv-sound", JSON.stringify(sound));
    } catch {
      // Storage blocked: remembered for this visit only.
    }
  },
  true,
);

// ---- The guide --------------------------------------------------------------

function describe(slot, link) {
  const title = link ? `<a href="${slot.href}">${escape(slot.title)}</a>` : escape(slot.title);
  const who = slot.author ? ` · ${escape(slot.author)}` : "";
  return `${title}<span class="tv-who">${who} · ${slot.platform}</span>`;
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

// ---- Turning on -------------------------------------------------------------

// Browsers let a page play sound once the viewer has interacted with the
// site; otherwise the power button waits for a click. Web Audio says which:
// it starts suspended, and resumes by itself only where sound is allowed.
tune(channel);
(async () => {
  if (audio) await Promise.race([audio.resume().catch(() => {}), new Promise((r) => setTimeout(r, 500))]);
  if (audio ? audio.state === "running" : navigator.userActivation?.hasBeenActive) {
    powered = true;
    if (onAir) play(onAir);
  } else {
    turnedOff();
  }
})();
