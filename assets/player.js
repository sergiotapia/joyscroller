// The player's own behavior (src/player.rs builds its markup; Video.js runs
// the controls): the photo slideshow, starting a photo post's sound when the
// browser held it back, and saying so when a file can't play here.

for (const show of document.querySelectorAll(".slideshow")) slideshow(show);

// The pictures sit side by side in a strip that slides along: the arrows,
// the dots and the arrow keys turn one picture, and dragging pulls the
// strip with the pointer, which lets go onto the nearest picture. A quick
// flick turns the page even when it's short.
function slideshow(show) {
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
  }

  prev.addEventListener("click", () => go(index - 1));
  next.addEventListener("click", () => go(index + 1));
  dots.forEach((dot, i) => dot.addEventListener("click", () => go(i)));
  addEventListener("keydown", (e) => {
    if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey) return;
    // The seek and volume sliders use the arrow keys themselves.
    if (e.target.closest?.("[role=slider], input, textarea")) return;
    const step = { ArrowLeft: -1, ArrowRight: 1 }[e.key];
    if (!step) return;
    e.preventDefault();
    go(index + step);
  });

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
  const fit = () => {
    if (first.naturalWidth) show.style.aspectRatio = `${first.naturalWidth} / ${first.naturalHeight}`;
  };
  if (first.complete) fit();
  else first.addEventListener("load", fit, { once: true });
}

// A photo post's sound plays by itself, as on TikTok. Browsers allow that
// only once the user has interacted with the site (arriving from the list
// counts), so when it's held back, the first click or arrow key on the page
// starts it. A click on the strip's own controls is left to them: starting
// the sound there would have the play button pause it straight away.
for (const audio of document.querySelectorAll("audio[autoplay]")) {
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
  addEventListener("pointerdown", start, true);
  addEventListener("keydown", start, true);
  audio.addEventListener("playing", stop, { once: true });
}

// A file the browser can't decode fails with an error, except a video whose
// sound it can play: then it plays the sound over a blank screen, which is
// what Chromium does with TikTok's HEVC when the GPU can't decode it.
for (const media of document.querySelectorAll(".player video, .player audio")) {
  const note = media.closest(".player").querySelector(".player-error");
  const show = () => (note.hidden = false);
  const noPicture = () => media.localName === "video" && media.readyState >= 1 && media.videoWidth === 0;
  if (media.error || noPicture()) show();
  media.addEventListener("error", show);
  media.addEventListener("loadedmetadata", () => noPicture() && show());
}
