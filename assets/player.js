// The player's own behavior (src/player.rs builds its markup; Video.js runs
// the controls): picking between a video's original and its playback copy,
// the photo slideshow (with a carousel's videos among its pictures),
// starting a photo post's sound when the browser held it back, and saying so
// when a file can't play here.
//
// A video's page has one player, set up as it loads. /tv (tv.js) puts
// players in and takes them out as shows come and go: it sets each up with
// `setUp`, and aborting `signal` takes away the listeners it added outside
// the player. A player that gives up on its file fires `player-failed`.

setUp(document);

export function setUp(root, signal) {
  for (const show of root.querySelectorAll(".slideshow")) slideshow(show, signal);
  for (const audio of root.querySelectorAll("audio[autoplay]")) startWhenAllowed(audio, signal);
  for (const video of root.querySelectorAll(".player video[data-copy]")) pick(video);
  for (const media of root.querySelectorAll(".player video:not(.slide-video), .player audio")) watchFailure(media);
}

// The pictures sit side by side in a strip that slides along: the arrows,
// the dots and the arrow keys turn one picture, and dragging pulls the
// strip with the pointer, which lets go onto the nearest picture. A quick
// flick turns the page even when it's short. A video among the pictures (an
// Instagram carousel's) plays while it's shown and pauses when it isn't.
function slideshow(show, signal) {
  const strip = show.querySelector(".slides");
  const pictures = [...strip.children];
  const dots = [...show.querySelectorAll(".slide-dot")];
  const prev = show.querySelector(".slide-prev");
  const next = show.querySelector(".slide-next");
  const at = show.querySelector(".slide-at");
  const last = pictures.length - 1;
  let index = 0;

  function go(to) {
    index = Math.max(0, Math.min(last, to));
    strip.style.transition = "";
    strip.style.transform = `translateX(${-index * 100}%)`;
    prev.disabled = index === 0;
    next.disabled = index === last;
    at.textContent = index + 1;
    dots.forEach((dot, i) => (i === index ? dot.setAttribute("aria-current", "true") : dot.removeAttribute("aria-current")));
    playShown();
  }

  // With sound, a video held back by the browser's autoplay rules plays
  // muted rather than not at all.
  function playShown() {
    for (const video of pictures.filter((p) => p.localName === "video")) {
      if (video !== pictures[index]) {
        video.pause();
      } else if (video.paused) {
        video.play().catch(() => {
          video.muted = true;
          video.play().catch(() => {});
        });
      }
    }
  }
  playShown();

  prev.addEventListener("click", () => go(index - 1));
  next.addEventListener("click", () => go(index + 1));
  dots.forEach((dot, i) => dot.addEventListener("click", () => go(i)));
  addEventListener("keydown", (e) => {
    if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey) return;
    // A player waiting off screen, as /tv's next show does, isn't turned.
    if (show.closest("[inert]")) return;
    // The seek and volume sliders use the arrow keys themselves.
    if (e.target.closest?.("[role=slider], input, textarea")) return;
    const step = { ArrowLeft: -1, ArrowRight: 1 }[e.key];
    if (!step) return;
    e.preventDefault();
    go(index + step);
  }, { signal });

  let drag = null;
  strip.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || pictures.length < 2) return;
    drag = { id: e.pointerId, x: e.clientX, at: e.timeStamp, dx: 0 };
    strip.setPointerCapture(e.pointerId);
    strip.style.transition = "none";
    show.classList.add("dragging");
  });
  strip.addEventListener("pointermove", (e) => {
    if (drag?.id !== e.pointerId) return;
    let dx = e.clientX - drag.x;
    // Past the first or last picture the strip gives way only a little.
    if ((index === 0 && dx > 0) || (index === last && dx < 0)) dx /= 4;
    drag.dx = dx;
    strip.style.transform = `translateX(calc(${-index * 100}% + ${dx}px))`;
  });
  const letGo = (e) => {
    if (drag?.id !== e.pointerId) return;
    const { dx, at: from } = drag;
    drag = null;
    show.classList.remove("dragging");
    const speed = Math.abs(dx) / Math.max(1, e.timeStamp - from);
    const far = Math.abs(dx) > show.clientWidth / 4;
    const flick = speed > 0.4 && Math.abs(dx) > 24;
    go(far || flick ? index - Math.sign(dx) : index);
  };
  strip.addEventListener("pointerup", letGo);
  strip.addEventListener("pointercancel", letGo);

  // The screen takes the first picture's shape (TikTok's are mostly 3:4 or
  // 9:16), so it isn't letterboxed. Later ones that differ are fitted inside.
  const first = pictures[0];
  if (first.localName === "video") {
    const fit = () => {
      if (first.videoWidth) show.style.aspectRatio = `${first.videoWidth} / ${first.videoHeight}`;
    };
    if (first.readyState >= 1) fit();
    else first.addEventListener("loadedmetadata", fit, { once: true });
  } else {
    const fit = () => {
      if (first.naturalWidth) show.style.aspectRatio = `${first.naturalWidth} / ${first.naturalHeight}`;
    };
    if (first.complete) fit();
    else first.addEventListener("load", fit, { once: true });
  }
}

// A photo post's sound plays by itself, as on TikTok. Browsers allow that
// only once the user has interacted with the site (arriving from the list
// counts), so when it's held back, the first click or arrow key on the page
// starts it. A click on the strip's own controls is left to them: starting
// the sound there would have the play button pause it straight away.
function startWhenAllowed(audio, signal) {
  const start = (e) => {
    if (e.type === "keydown" && !["ArrowLeft", "ArrowRight"].includes(e.key)) return;
    stop();
    if (e.target.closest?.(".player-strip")) return;
    if (audio.paused && audio.played.length === 0) audio.play().catch(() => {});
  };
  const stop = () => {
    removeEventListener("pointerdown", start, true);
    removeEventListener("keydown", start, true);
  };
  addEventListener("pointerdown", start, { capture: true, signal });
  addEventListener("keydown", start, { capture: true, signal });
  audio.addEventListener("playing", stop, { once: true });
}

// A video with a playback copy (H.264, plays everywhere) next to its
// original (often AV1, HEVC or VP9) plays the original only if the browser
// says it decodes it smoothly and power-efficiently, which in practice means
// in hardware. "Can play" isn't enough: a Tesla or an older phone can decode
// AV1 in software and stutters through it, draining the battery.

async function pick(video) {
  let original = false;
  let decodes = true;
  try {
    const config = JSON.parse(video.dataset.source);
    const info = await navigator.mediaCapabilities.decodingInfo({ type: "file", video: config });
    original = info.supported && info.smooth && info.powerEfficient;
    decodes = info.supported;
  } catch {
    // No MediaCapabilities, or no description of the original: the copy.
  }
  use(video, original ? video.dataset.original : video.dataset.copy);
  // A switch to an original this browser can't decode would only fall back.
  const button = document.getElementById("player-pick");
  if (!button || !decodes) return;
  const label = () => {
    const playing = video.getAttribute("src") === video.dataset.original;
    button.textContent = playing ? "Play the playback copy" : "Play the original";
  };
  label();
  button.hidden = false;
  button.addEventListener("click", () => {
    const playing = video.getAttribute("src") === video.dataset.original;
    use(video, playing ? video.dataset.copy : video.dataset.original);
    label();
  });
  video.addEventListener("emptied", label);
}

// Switch `video` to the file at `src`, carrying on from the same moment.
function use(video, src) {
  if (video.getAttribute("src") === src) return;
  const at = video.currentTime;
  const playing = !video.paused;
  video.closest(".player").querySelector(".player-error").hidden = true;
  video.src = src;
  if (at > 0) video.addEventListener("loadedmetadata", () => (video.currentTime = at), { once: true });
  if (playing) video.play().catch(() => {});
}

// A file the browser can't decode fails with an error, except a video whose
// sound it can play: then it plays the sound over a blank screen, which is
// what Chromium does with TikTok's HEVC when the GPU can't decode it. A
// video with a playback copy switches to it. When nothing plays (no copy
// yet, or its file has gone), the page asks for one and reloads into it
// once it's made.
// A slideshow's videos are left out: a slide that won't play just stays dark.
function watchFailure(media) {
  const note = media.closest(".player").querySelector(".player-error");
  const failed = () => {
    const copy = media.dataset.copy;
    if (copy && media.getAttribute("src") !== copy) return use(media, copy);
    if (!note.hidden) return;
    note.hidden = false;
    media.dispatchEvent(new Event("player-failed", { bubbles: true }));
    if (media.dataset.convert) {
      fetch(media.dataset.convert, { method: "POST" })
        .then((response) => {
          if (response.ok) note.querySelector(".player-converting").hidden = false;
        })
        .catch(() => {});
    }
  };
  const noPicture = () => media.localName === "video" && media.readyState >= 1 && media.videoWidth === 0;
  if (media.error || noPicture()) failed();
  media.addEventListener("error", failed);
  media.addEventListener("loadedmetadata", () => noPicture() && failed());
}
