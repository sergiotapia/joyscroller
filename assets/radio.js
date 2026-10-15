// The side panel's clock radio comes alive while the pointer is on it (or
// the keyboard's focus): its tube switches on with electricity running
// through it, the alarm dome bounces and the tuning knob spins.
//
// One "power" level, rising on hover and falling after, drives all of it,
// so leaving halfway through winds down from wherever it got to: the glow
// fades, the dome's hops shrink until it settles, and the knob coasts on to
// its next resting angle. The moving parts are frames rendered from the
// same Blender scene as the picture (assets/blender/radio_motion.py), laid
// over it; radio.json, in data-motion, says where. Nothing runs while the
// radio is idle.

const radio = document.querySelector(".radio[data-motion]");
if (radio) start(radio, JSON.parse(radio.dataset.motion));

function start(radio, motion) {
  const calm = matchMedia("(prefers-reduced-motion: reduce)");
  const off = radio.querySelector(".radio-off");
  const on = radio.querySelector(".radio-on");
  const domes = [...radio.querySelectorAll(".radio-dome img")];
  const knobs = [...radio.querySelectorAll(".radio-knob img")];
  const arc = electricity(radio.querySelector(".radio-arc"), motion.tube);

  // Seconds for the power to come up, and to go down again.
  const RISE = 0.4;
  const FALL = 0.75;

  let wanted = 0;
  let hovered = false;
  let focused = false;
  let power = 0;
  let glow = 0;
  let struck = -1;
  let hum = 1;
  let humAt = 0;
  const dome = { height: 0, speed: 0, squashFor: 0, squash: 0, frame: 0 };
  const knob = { angle: 0, speed: 0, stopAt: null, frame: 0 };
  let last = 0;
  let running = false;

  function want() {
    wanted = hovered || focused ? 1 : 0;
    if (!running) {
      running = true;
      last = performance.now();
      requestAnimationFrame(tick);
    }
  }
  radio.addEventListener("pointerenter", (event) => {
    // A tap is a click on the link, not a hover.
    if (event.pointerType === "touch") return;
    hovered = true;
    want();
  });
  radio.addEventListener("pointerleave", () => {
    hovered = false;
    want();
  });
  radio.addEventListener("focus", () => {
    focused = radio.matches(":focus-visible");
    want();
  });
  radio.addEventListener("blur", () => {
    focused = false;
    want();
  });

  // The lit layers sit at opacity 0 until the first hover, where browsers
  // may leave them undecoded: decode them now, so it starts smoothly.
  const decode = () => {
    for (const img of radio.querySelectorAll("img")) img.decode().catch(() => {});
  };
  if (window.requestIdleCallback) requestIdleCallback(decode);
  else setTimeout(decode, 500);

  function tick(now) {
    const dt = Math.min((now - last) / 1000, 0.05);
    last = now;
    const t = now / 1000;
    const rising = wanted > power;
    power = rising ? Math.min(wanted, power + dt / RISE) : Math.max(wanted, power - dt / FALL);
    const eased = power * power * (3 - 2 * power);

    if (calm.matches) {
      // Reduced motion: the tube glows, steadily; nothing moves.
      glow = eased;
      setGlow(glow);
      arc.show(glow, t, false);
      setFrame(domes, motion.dome, 0);
      setFrame(knobs, motion.knob, 0);
      return next(power > 0 || wanted > 0);
    }

    // The glow: a fluorescent starter's misfires coming up from cold, then
    // a steady light with an electric shimmer.
    if (rising && glow < 0.05 && struck < 0) struck = t;
    if (!rising && power === 0) struck = -1;
    if (t - humAt > 0.06 + Math.random() * 0.05) {
      hum = 0.86 + Math.random() * 0.14;
      humAt = t;
    }
    // The misfires flash at their own brightness, while the power is still
    // coming up; leaving cuts them short.
    const starting = struck >= 0 && rising && t - struck < STRIKE;
    glow = (starting ? strike(t - struck) : eased) * hum;
    setGlow(glow);
    arc.show(glow, t, true);

    bounce(dt);
    spin(dt);
    return next(power > 0 || wanted > 0 || dome.frame !== 0 || knob.speed > 0);
  }

  function next(more) {
    if (more) requestAnimationFrame(tick);
    else {
      running = false;
      struck = -1;
      setGlow(0);
      arc.show(0, 0, false);
    }
  }

  function setGlow(level) {
    off.style.opacity = 1 - level;
    on.style.opacity = level;
  }

  // The dome hops under gravity, in units of its highest pose. Each landing
  // squashes it into its collar for a moment, then the power sends it up
  // again, higher the more power there is, and never the same twice.
  const GRAVITY = 55;
  const SQUASH = 0.07;
  function bounce(dt) {
    if (dome.squashFor > 0) {
      dome.squashFor -= dt;
      if (dome.squashFor <= 0) {
        dome.squash = 0;
        if (power > 0.3) {
          const height = power * (0.35 + Math.random() * 0.65);
          dome.speed = Math.sqrt(2 * GRAVITY * height);
          dome.height = 0.0001;
        }
      }
    } else if (dome.height > 0) {
      dome.speed -= GRAVITY * dt;
      dome.height += dome.speed * dt;
      if (dome.height <= 0) {
        // Landing: squashed in proportion to how hard it came down.
        dome.squash = Math.min(1, -dome.speed / Math.sqrt(2 * GRAVITY));
        dome.squashFor = SQUASH;
        dome.height = 0;
        dome.speed = 0;
      }
    } else if (power > 0.3 && wanted > 0) {
      // The first hop, once the power is up: the dome goes after the tube.
      dome.squash = 0.4;
      dome.squashFor = SQUASH;
    }
    const { squashes, lifts } = motion.dome;
    let frame = 0;
    if (dome.squashFor > 0) {
      // Deepest halfway through the landing.
      const depth = dome.squash * Math.sin(Math.PI * (1 - dome.squashFor / SQUASH));
      frame = Math.round(depth * squashes);
    } else if (dome.height > 0) {
      const step = Math.round(Math.min(dome.height, 1) * lifts);
      frame = step > 0 ? squashes + step : 0;
    }
    dome.frame = frame;
    setFrame(domes, motion.dome, frame);
  }

  // The knob spins up with the power, and when the power goes it coasts on
  // to its next resting angle (each 60 degrees, where its six lobes line up
  // as they started) and stops there.
  const TOP_SPEED = 320;
  function spin(dt) {
    if (wanted > 0) {
      knob.stopAt = null;
      knob.speed += (power * TOP_SPEED - knob.speed) * Math.min(1, dt / 0.3);
    } else if (knob.speed > 0) {
      if (knob.stopAt === null) {
        // Coast at least a quarter of a second's worth, then decelerate
        // evenly to the next rest.
        knob.stopAt = Math.ceil((knob.angle + knob.speed * 0.25) / 60) * 60;
        knob.brake = knob.speed ** 2 / (2 * (knob.stopAt - knob.angle));
      }
      knob.speed = Math.max(0, knob.speed - knob.brake * dt);
      if (knob.speed === 0 || knob.angle >= knob.stopAt) {
        knob.angle = 0;
        knob.speed = 0;
        knob.stopAt = null;
      }
    }
    knob.angle = (knob.angle + knob.speed * dt) % (knob.stopAt === null ? 60 : Infinity);
    if (knob.stopAt !== null && knob.angle > knob.stopAt) knob.angle = knob.stopAt;
    const frames = motion.knob.frames;
    knob.frame = Math.floor((knob.angle % 60) / (60 / frames)) % frames;
    setFrame(knobs, motion.knob, knob.frame);
  }
}

// A fluorescent tube's start, STRIKE seconds long: two misfires, then up
// to full.
const STRIKE = 0.4;
function strike(t) {
  if (t < 0.05) return 0.75;
  if (t < 0.11) return 0.06;
  if (t < 0.16) return 0.95;
  if (t < 0.24) return 0.15;
  return Math.min(1, 0.45 + ((t - 0.24) / (STRIKE - 0.24)) * 0.55);
}

// Shows a frame of a sprite sheet: the layer's <img> is the whole sheet,
// moved so the frame's tile fills the layer's box.
function setFrame(imgs, sheet, frame) {
  const { box, cols, rows, gutter } = sheet;
  const pitchX = box[2] + 2 * gutter;
  const pitchY = box[3] + 2 * gutter;
  const x = ((gutter + (frame % cols) * pitchX) / (cols * pitchX)) * 100;
  const y = ((gutter + Math.floor(frame / cols) * pitchY) / (rows * pitchY)) * 100;
  const transform = `translate(${-x}%, ${-y}%)`;
  for (const img of imgs) if (img.style.transform !== transform) img.style.transform = transform;
}

// The arc in the tube: two filaments wandering inside it, redrawn a few
// dozen times a second so they crackle, and a surge of brightness running
// from the ball to the fitting.
function electricity(svg, tube) {
  const paths = [...svg.querySelectorAll("path")];
  const [glowPath, corePath, hotPath, surgePath] = paths;
  const line = resample(tube.points, 6);
  const room = tube.radius * 0.9;
  let drawnAt = -1;
  let surgeFrom = 0;
  let surgeFor = 0.7;
  let shown = false;

  return {
    show(level, t, moving) {
      if (level < 0.02) {
        if (shown) {
          svg.style.visibility = "hidden";
          shown = false;
          drawnAt = -1;
        }
        return;
      }
      if (!shown) {
        svg.style.visibility = "visible";
        shown = true;
      }
      svg.style.opacity = Math.min(1, level * 1.15);
      if (drawnAt < 0 || (moving && t - drawnAt > 0.04 + Math.random() * 0.04)) {
        const main = filament(line, room);
        glowPath.setAttribute("d", main);
        corePath.setAttribute("d", main);
        hotPath.setAttribute("d", filament(line, room * 0.8));
        surgePath.setAttribute("d", main);
        drawnAt = t;
      }
      if (!moving) {
        surgePath.style.visibility = "hidden";
        return;
      }
      surgePath.style.visibility = "visible";
      let along = (t - surgeFrom) / surgeFor;
      if (along > 1.3) {
        surgeFrom = t;
        surgeFor = 0.45 + Math.random() * 0.35;
        along = 0;
      }
      // pathLength is 1: a dash a seventh of the tube long, moving along it.
      surgePath.style.strokeDashoffset = 0.15 - along * 1.3;
    },
  };
}

// Points every `step` pixels along a polyline, each with the unit normal
// there.
function resample(points, step) {
  const lengths = [0];
  for (let i = 1; i < points.length; i++) {
    const [ax, ay] = points[i - 1];
    const [bx, by] = points[i];
    lengths.push(lengths[i - 1] + Math.hypot(bx - ax, by - ay));
  }
  const total = lengths[lengths.length - 1];
  const count = Math.max(2, Math.round(total / step));
  const out = [];
  let seg = 1;
  for (let k = 0; k <= count; k++) {
    const s = (total * k) / count;
    while (seg < points.length - 1 && lengths[seg] < s) seg++;
    const [ax, ay] = points[seg - 1];
    const [bx, by] = points[seg];
    const span = lengths[seg] - lengths[seg - 1] || 1;
    const f = (s - lengths[seg - 1]) / span;
    const nx = -(by - ay) / span;
    const ny = (bx - ax) / span;
    out.push([ax + (bx - ax) * f, ay + (by - ay) * f, nx, ny]);
  }
  return out;
}

// A jagged path along the line, kept within `room` of it: midpoint
// displacement, pinned at both ends, coarse bends with finer kinks on them.
function filament(line, room) {
  const n = line.length;
  const offset = new Float32Array(n);
  const split = (i, j, size) => {
    if (j - i < 2) return;
    const m = (i + j) >> 1;
    offset[m] = (offset[i] + offset[j]) / 2 + (Math.random() * 2 - 1) * size;
    offset[m] = Math.max(-room, Math.min(room, offset[m]));
    split(i, m, size * 0.55);
    split(m, j, size * 0.55);
  };
  split(0, n - 1, room * 1.6);
  let d = "";
  for (let k = 0; k < n; k++) {
    const [x, y, nx, ny] = line[k];
    d += (k ? "L" : "M") + (x + nx * offset[k]).toFixed(1) + " " + (y + ny * offset[k]).toFixed(1);
  }
  return d;
}
