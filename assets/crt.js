// /tv's picture tube (tv.js runs it): draws what's on air the way an early-
// 90s color set showed broadcast NTSC, in WebGL2, from the video, picture or
// graphics it's given. Two passes:
//
// 1. The signal, at the broadcast raster's 640 x 480: the picture fitted
//    into 4:3 (letterboxed or pillarboxed, as stations did), the station's
//    graphics keyed over it, then a composite encode and decode. NTSC sends
//    color as two narrow-band signals riding on the brightness, I and Q
//    (SMPTE 170M: luma to 4.2 MHz, I to 1.3 MHz, Q to 0.4 MHz, where 640
//    samples span the 52.6 us line at 12.2 MHz), so color smears sideways
//    while edges stay sharp. What a set's comb filter misses of the 3.58 MHz
//    subcarrier crawls over saturated edges as dots, and its sharpness
//    circuit rings at edges. Then snow (no signal) and a picture rolling
//    while the set locks on to a new channel, and the set's own on-screen
//    display (channel, volume), which it adds after decoding.
// 2. The tube, over the face (see the pass below): a bowed face with
//    rounded corners, 240 lines of a sharp beam whose lines close up as it
//    brightens, a dot mask and the tube's gamma, and the set's power-off
//    collapse to a dot and degaussing wobble.
//
// A face with fewer pixels than the raster has lines is drawn at twice the
// raster's height, off screen, and scaled down into place, so the lines and
// the mask aren't drawn finer than they can be.

const RASTER_W = 640;
const RASTER_H = 480;

const quad = `#version 300 es
in vec2 at;
out vec2 uv;
void main() {
  uv = at * 0.5 + 0.5;
  gl_Position = vec4(at, 0.0, 1.0);
}`;

// Pass 1a: the picture, fitted into the raster, with graphics, as YIQ.
const fitSource = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 color;
uniform sampler2D source;
uniform sampler2D graphics;
uniform vec4 fit;       // where the picture sits in the raster: x, y, w, h (0-1)
uniform float hasSource;
uniform float hasGraphics;
uniform float roll;     // how far the picture has rolled, in frames
uniform float time;
uniform float jitter;   // horizontal sync wobble, in raster pixels

const mat3 toYiq = mat3(0.299, 0.596, 0.211, 0.587, -0.274, -0.523, 0.114, -0.322, 0.312);

float hash(vec2 p) { return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }

void main() {
  // Top of the raster first.
  vec2 p = vec2(uv.x, 1.0 - uv.y);
  float line = floor(p.y * ${RASTER_H}.0);
  // While the picture rolls, the vertical blanking interval shows as a
  // black bar travelling with it.
  float y = p.y;
  float blank = 1.0;
  if (roll != 0.0) {
    y = fract(p.y + roll);
    blank = smoothstep(0.0, 0.01, y) * smoothstep(0.0, 0.01, 0.94 - y);
    y = y / 0.94;
  }
  float x = p.x + jitter * (hash(vec2(line, floor(time * 60.0))) - 0.5) / ${RASTER_W}.0;
  vec3 rgb = vec3(0.0);
  vec2 inPic = (vec2(x, y) - fit.xy) / fit.zw;
  if (hasSource > 0.5 && all(greaterThanEqual(inPic, vec2(0.0))) && all(lessThanEqual(inPic, vec2(1.0)))) {
    rgb = texture(source, inPic).rgb;
  }
  if (hasGraphics > 0.5) {
    vec4 g = texture(graphics, vec2(x, y));
    rgb = rgb * (1.0 - g.a) + g.rgb;
  }
  // Broadcast levels: white held to 235 of 255 (the day's off-air
  // recordings top out between 190 and 235), black lifted a hair, as a set
  // in a lit room shows it.
  rgb = mix(vec3(0.025), vec3(0.92), rgb) * blank;
  color = vec4(toYiq * rgb, 1.0);
}`;

// Pass 1b: decode: band-limit Y, I and Q along the line, leave a little of
// the subcarrier in the luma, ring at edges, add the set's OSD and snow.
const decode = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 color;
uniform sampler2D yiq;
uniform sampler2D osd;
uniform float hasOsd;
uniform float frame;
uniform float time;
uniform float snow;     // 0: clean signal, 1: nothing but snow
uniform float noise;    // a weak station's grain, always a little

const mat3 toRgb = mat3(1.0, 1.0, 1.0, 0.956, -0.272, -1.106, 0.621, -0.647, 1.703);
const float PI = 3.14159265;

float hash(vec2 p) { return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }

// Gaussian taps along the line, sigma in raster pixels.
vec3 lowpass(float dx, vec3 sigma) {
  vec3 sum = vec3(0.0);
  vec3 weight = vec3(0.0);
  for (int k = -10; k <= 10; k++) {
    float d = float(k);
    vec3 w = exp(-0.5 * d * d / (sigma * sigma));
    vec3 s = texture(yiq, uv + vec2(d * dx, 0.0)).rgb;
    sum += s * w;
    weight += w;
  }
  return sum / weight;
}

void main() {
  float dx = 1.0 / ${RASTER_W}.0;
  // At 12.2 MHz, a Gaussian's -3 dB point is at sigma = 0.1325 fs / fc.
  // SMPTE 170M sends luma to 4.2 MHz, I to 1.3 and Q to 0.4; a consumer set
  // of the day decoded both I and Q at about 0.55 MHz and luma at about
  // 2 (its notch filter taking the top off). That smears color about five
  // times as far as brightness, the ratio measured in off-air recordings of
  // the time (whose tape doubled both).
  vec3 sharp = lowpass(dx, vec3(0.8, 2.9, 3.2));
  // The set's sharpness circuit: luma minus a blur of itself, added back,
  // overshooting a bright edge by about 20 of 255, as recordings show.
  float soft = lowpass(dx, vec3(1.8, 1.8, 1.8)).r;
  float y = sharp.r + 0.6 * (sharp.r - soft);
  vec2 iq = sharp.gb;
  // Dot crawl: the 3.58 MHz subcarrier (227.5 cycles a line, so it flips
  // phase every line and every frame) left in the luma where color
  // changes, which a notch filter doesn't catch.
  float line = floor((1.0 - uv.y) * ${RASTER_H}.0);
  float phase = 2.0 * PI * (uv.x * ${RASTER_W}.0 * 0.2934) + PI * (line + frame);
  vec2 edge = texture(yiq, uv + vec2(dx, 0.0)).gb - texture(yiq, uv - vec2(dx, 0.0)).gb;
  y += 0.18 * dot(edge, vec2(cos(phase), sin(phase)));

  // Noise: grain stretched along the line (the receiver's bandwidth), in
  // streaks, as recordings of the day show it, the color's about half the
  // brightness's. At full strength it's snow: no station at all.
  float t = floor(time * 60.0);
  float grain = hash(vec2(floor(uv.x * ${RASTER_W}.0 / 3.0), line + 517.0 * fract(t * 0.1713)));
  grain = mix(grain, hash(vec2(floor(uv.x * ${RASTER_W}.0 / 1.3), line + 91.0 * fract(t * 0.377))), 0.45);
  float streak = hash(vec2(line, t)) - 0.5;
  y = mix(y, grain * 1.05, snow) + ((grain - 0.5) * 0.7 + streak * 0.6) * noise;
  iq = iq * (1.0 - snow) + (vec2(hash(vec2(line, t + 0.5)), hash(vec2(t, line))) - 0.5) * mix(noise * 0.5, 0.08, snow);

  vec3 rgb = clamp(toRgb * vec3(y, iq), 0.0, 1.0);
  if (hasOsd > 0.5) {
    vec4 o = texture(osd, vec2(uv.x, 1.0 - uv.y));
    rgb = rgb * (1.0 - o.a) + o.rgb;
  }
  color = vec4(rgb, 1.0);
}`;

// Pass 2: the tube, drawn over the face. The face is part of a sphere seen
// from a little way off, so the picture bows out to the face's edges and
// its corners fall short, rounded, into black. Each line
// is resampled sharply along its length (Lanczos2); the beam that draws it
// is a bell that flattens and widens as it brightens, so bright lines close
// up and dark ones leave gaps, averaged over each pixel's height so lines
// finer than the pixels blend rather than shimmer. The 480-line picture is
// drawn as 240 lines, each from a pair of the picture's: both fields at
// once, as the phosphor's glow blended them. (Drawing them a frame each, as
// the beam did, flickers on a display without that glow.)
// A magenta and green dot mask, the tube's gamma (2.4) out to the
// display's (2.2). On top, the set: the power-off collapse to a line and a
// dot, and the degaussing coil's purity wobble at power-on.
const tube = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 color;
uniform sampler2D signal;
uniform float power;     // the beam's strength: 1 on, 0 off (and dips between shows)
uniform float collapse;  // the power-off collapse, 0 to 1
uniform float degauss;   // 1 at power-on, fading to 0
uniform float time;

const float PI = 3.14159265;
// The face's size, one wide and 4:3, the unit the sphere's radius and the
// viewing distance are in.
const vec2 FACE = vec2(1.0, 0.75);
const float RADIUS = 2.0;
const float DISTANCE = 1.6;
const float CORNER = 0.03;
const float CORNER_SHARPNESS = 1000.0;
const float MASK = 0.3;
// The beam's width, in line spacings, and the light its lines give of the
// flat picture's.
const float BEAM = 0.3;
const float LINE_LIGHT = 0.8;
const float TUBE_GAMMA = 2.4;
const float DISPLAY_GAMMA = 2.2;

// Where the eye's ray through q (on the plane at the face's front) lands on
// the sphere: the arc lengths to it, across and up.
vec2 arcs(vec2 q) {
  vec3 center = vec3(0.0, 0.0, -RADIUS);
  vec3 eye = vec3(0.0, 0.0, DISTANCE);
  vec3 ray = normalize(vec3(q, -DISTANCE));
  vec3 from = eye - center;
  float b = dot(from, ray);
  float disc = b * b - (dot(from, from) - RADIUS * RADIUS);
  if (disc < 0.0) return vec2(1e6);
  vec3 n = (eye + (-b - sqrt(disc)) * ray - center) / RADIUS;
  return RADIUS * vec2(atan(n.x, n.z), atan(n.y, n.z));
}

// How much of the picture shows at t (0-1): all of it inside, rounded off
// at the corners, nothing past its edges.
float corners(vec2 t) {
  vec2 from = (0.5 - abs(t - 0.5)) * FACE;
  vec2 into = max(vec2(CORNER) - from, 0.0);
  return clamp((CORNER - length(into)) * CORNER_SHARPNESS, 0.0, 1.0);
}

float sinc(float x) {
  return abs(x) < 1e-4 ? 1.0 : sin(PI * x) / (PI * x);
}

// Line "row" of the signal at x (in texels), in the tube's light: Lanczos2
// over its four nearest pixels.
vec3 line(int row, float x) {
  float left = floor(x - 0.5);
  float f = x - 0.5 - left;
  vec3 sum = vec3(0.0);
  float total = 0.0;
  for (int k = -1; k <= 2; k++) {
    float dist = abs(float(k) - f);
    float w = dist < 2.0 ? sinc(dist) * sinc(dist * 0.5) : 0.0;
    ivec2 at = ivec2(clamp(int(left) + k, 0, ${RASTER_W} - 1), clamp(row, 0, ${RASTER_H} - 1));
    sum += w * pow(max(texelFetch(signal, at, 0).rgb, 0.0), vec3(TUBE_GAMMA));
    total += w;
  }
  return max(sum / total, 0.0);
}

// The beam's light at "dist" line spacings from its center, for a line of
// brightness c: a bell whose exponent goes from 2 to 4 as it brightens,
// scaled by its area (2 sigma Gamma(1 + 1/e), Gamma running 0.886 to
// 0.906), so a line gives the same light whatever its shape.
vec3 beam(float dist, vec3 c) {
  vec3 e = 2.0 + 2.0 * pow(c, vec3(4.0));
  vec3 w = exp(-pow(vec3(abs(dist) / BEAM), e));
  return w / (2.0 * BEAM * (0.886 + 0.0101 * (e - 2.0)));
}

void main() {
  // Collapsing: the raster squeezes to a line, then to a dot.
  float squeezeY = mix(1.0, 0.006, smoothstep(0.0, 0.35, collapse));
  float squeezeX = mix(1.0, 0.012, smoothstep(0.35, 0.55, collapse));
  vec2 at = (uv - 0.5) / vec2(squeezeX, squeezeY) + 0.5;
  vec2 c = uv * 2.0 - 1.0;

  // The face's own dark gray where no beam lands, under the room's light.
  vec3 glass = vec3(0.0035, 0.0045, 0.004);
  vec3 light = vec3(0.0);
  vec2 edge = vec2(arcs(vec2(FACE.x * 0.5, 0.0)).x, arcs(vec2(0.0, FACE.y * 0.5)).y);
  vec2 t = arcs((at - 0.5) * FACE) / edge * 0.5 + 0.5;
  float shown = corners(t);
  if (shown > 0.0) {
    vec2 tc = t * vec2(${RASTER_W}.0, ${RASTER_H}.0);
    // Line k is the picture's lines 2k and 2k + 1, centered between them.
    float y = (tc.y - 1.0) / 2.0;
    float base = floor(y);
    float f = y - base;
    int row0 = int(base) * 2;
    vec3 c0 = 0.5 * (line(row0, tc.x) + line(row0 + 1, tc.x));
    vec3 c1 = 0.5 * (line(row0 + 2, tc.x) + line(row0 + 3, tc.x));
    float reach = (abs(dFdx(tc.y)) + abs(dFdy(tc.y))) / 6.0;
    for (int k = -1; k <= 1; k++) {
      float d = f + float(k) * reach;
      light += c0 * beam(d, c0) + c1 * beam(1.0 - d, c1);
    }
    light *= LINE_LIGHT / 3.0;
    float odd = mod(floor(gl_FragCoord.x), 2.0);
    light *= mix(vec3(1.0, 1.0 - MASK, 1.0), vec3(1.0 - MASK, 1.0, 1.0 - MASK), odd) * shown;
    // Collapsing, the beam's light only ever fades (so the screen never
    // brightens as a whole, which would flash), while the shrinking
    // picture gets brighter for its size, up to a point.
    float energy = 1.0 - smoothstep(0.05, 1.0, collapse);
    light *= min(energy / max(squeezeX * squeezeY, 1e-4), 2.5) * power;
  }
  if (collapse > 0.0) {
    // The dot's afterglow on the phosphor, fading with the beam.
    float energy = 1.0 - smoothstep(0.05, 1.0, collapse);
    float d = length(c * vec2(4.0 / 3.0, 1.0));
    light += vec3(0.85, 0.92, 1.0) * energy * energy * smoothstep(0.3, 0.55, collapse) * exp(-d * d / 0.0012) * 1.6;
  }
  // Purity: the degaussing coil's field bends the beams off their
  // phosphors as it dies away, washing patches of the picture with color.
  if (degauss > 0.0) {
    float a = time * 7.0;
    vec2 q = c * 1.6 + vec2(sin(a), cos(a * 0.8)) * 0.4;
    vec3 tint = 0.5 + 0.5 * cos(vec3(0.0, 2.1, 4.2) + q.x * 2.5 + q.y * 1.7 + a);
    light *= mix(vec3(1.0), tint * 1.6, degauss * 0.55);
  }
  color = vec4(pow(clamp(glass + light, 0.0, 1.0), vec3(1.0 / DISPLAY_GAMMA)), 1.0);
}`;

// The offscreen tube, drawn onto the face.
const copy = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 color;
uniform sampler2D picture;
void main() {
  color = texture(picture, uv);
}`;

function compile(gl, type, source) {
  const shader = gl.createShader(type);
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader));
  return shader;
}

function program(gl, fragment) {
  const p = gl.createProgram();
  gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, quad));
  gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, fragment));
  gl.bindAttribLocation(p, 0, "at");
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  const uniforms = {};
  for (let i = 0; i < gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS); i++) {
    const { name } = gl.getActiveUniform(p, i);
    uniforms[name] = gl.getUniformLocation(p, name);
  }
  return { p, u: uniforms };
}

function texture(gl, filter = gl.LINEAR) {
  const t = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  return t;
}

function target(gl, mipmaps) {
  const t = texture(gl, mipmaps ? gl.LINEAR_MIPMAP_LINEAR : gl.LINEAR);
  // YIQ goes negative, so the passes need a float target. A GPU that can't
  // render to one gets the plain players instead (the constructor throws).
  if (!gl.getExtension("EXT_color_buffer_float") && !gl.getExtension("EXT_color_buffer_half_float")) {
    throw new Error("crt: no float render target");
  }
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA16F, RASTER_W, RASTER_H, 0, gl.RGBA, gl.HALF_FLOAT, null);
  const fb = gl.createFramebuffer();
  gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
  gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, t, 0);
  if (gl.checkFramebufferStatus(gl.FRAMEBUFFER) !== gl.FRAMEBUFFER_COMPLETE) throw new Error("crt: no render target");
  return { t, fb };
}

export class Tube {
  // Throws where WebGL2 isn't available; the page then shows the players
  // as they are.
  constructor(canvas) {
    const gl = canvas.getContext("webgl2", { alpha: false, antialias: false, powerPreference: "high-performance" });
    if (!gl) throw new Error("crt: no WebGL2");
    this.gl = gl;
    this.canvas = canvas;
    this.fit = program(gl, fitSource);
    this.decode = program(gl, decode);
    this.tube = program(gl, tube);
    this.copy = program(gl, copy);
    this.yiq = target(gl, false);
    this.signal = target(gl, false);
    this.sourceTex = texture(gl);
    this.graphicsTex = texture(gl);
    this.osdTex = texture(gl);
    const buffer = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    this.frame = 0;
    this.sourceAspect = 4 / 3;
    this.hasSource = false;
  }

  // Upload `el`'s current picture (a video, image or canvas), or none.
  // Returns whether there was one to show.
  source(el) {
    const gl = this.gl;
    const w = el?.videoWidth || el?.naturalWidth || el?.width || 0;
    const h = el?.videoHeight || el?.naturalHeight || el?.height || 0;
    const ready = el && w && h && (el.localName !== "video" || el.readyState >= 2) && (el.localName !== "img" || el.complete);
    if (!ready) {
      this.hasSource = false;
      this.uploaded = null;
      return false;
    }
    // Only a new picture is uploaded: a video's frame changes at its own
    // rate (often 30 a second, under the display's 60), a photo once.
    const frame = el.localName === "video" ? el.currentTime : el.currentSrc || el.src || el;
    if (this.uploaded && this.uploaded.el === el && this.uploaded.frame === frame) return true;
    this.uploaded = { el, frame };
    gl.bindTexture(gl.TEXTURE_2D, this.sourceTex);
    try {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, el);
    } catch {
      this.hasSource = false;
      this.uploaded = null;
      return false;
    }
    this.sourceAspect = w / h;
    this.hasSource = true;
    return true;
  }

  // Upload a 640 x 480 canvas of graphics or OSD (premultiplied), or null.
  layer(which, canvas) {
    const gl = this.gl;
    this[which] = Boolean(canvas);
    if (!canvas) return;
    gl.bindTexture(gl.TEXTURE_2D, which === "graphics" ? this.graphicsTex : this.osdTex);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, canvas);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  }

  // Draw a frame. `s` is the set's state: power (0-1), collapse (0-1),
  // degauss (0-1), snow (0-1), roll (frames), jitter (px), noise, and face
  // (the tube's face in canvas pixels, x, y, w, h from the top left).
  draw(s, time) {
    const gl = this.gl;
    this.frame++;
    // The picture, fitted into the 4:3 raster.
    const raster = RASTER_W / RASTER_H;
    let fit = [0, 0, 1, 1];
    if (this.sourceAspect > raster) {
      const h = raster / this.sourceAspect;
      fit = [0, (1 - h) / 2, 1, h];
    } else {
      const w = this.sourceAspect / raster;
      fit = [(1 - w) / 2, 0, w, 1];
    }
    gl.viewport(0, 0, RASTER_W, RASTER_H);
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.yiq.fb);
    gl.useProgram(this.fit.p);
    let u = this.fit.u;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.sourceTex);
    gl.uniform1i(u.source, 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.graphicsTex);
    gl.uniform1i(u.graphics, 1);
    gl.uniform4fv(u.fit, fit);
    gl.uniform1f(u.hasSource, this.hasSource ? 1 : 0);
    gl.uniform1f(u.hasGraphics, this.graphics ? 1 : 0);
    gl.uniform1f(u.roll, s.roll || 0);
    gl.uniform1f(u.time, time);
    gl.uniform1f(u.jitter, s.jitter || 0);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

    gl.bindFramebuffer(gl.FRAMEBUFFER, this.signal.fb);
    gl.useProgram(this.decode.p);
    u = this.decode.u;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.yiq.t);
    gl.uniform1i(u.yiq, 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.osdTex);
    gl.uniform1i(u.osd, 1);
    gl.uniform1f(u.hasOsd, this.osd ? 1 : 0);
    gl.uniform1f(u.frame, this.frame);
    gl.uniform1f(u.time, time);
    gl.uniform1f(u.snow, s.snow || 0);
    gl.uniform1f(u.noise, s.noise ?? 0.04);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);

    // The tube, over the face: straight onto the canvas, or, on a face
    // shorter than one pixel a line, at twice the raster's height off
    // screen, scaled down into place.
    const [fx, fy, fw, fh] = s.face.map(Math.round);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    const out = fh < RASTER_H * 1.5 ? this.offscreen(Math.round((RASTER_H * 2 * fw) / fh), RASTER_H * 2) : null;
    if (out) {
      gl.bindFramebuffer(gl.FRAMEBUFFER, out.fb);
      gl.viewport(0, 0, out.w, out.h);
    } else {
      gl.viewport(fx, this.canvas.height - fy - fh, fw, fh);
    }
    gl.useProgram(this.tube.p);
    u = this.tube.u;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.signal.t);
    gl.uniform1i(u.signal, 0);
    gl.uniform1f(u.power, s.power);
    gl.uniform1f(u.collapse, s.collapse || 0);
    gl.uniform1f(u.degauss, s.degauss || 0);
    gl.uniform1f(u.time, time);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    if (out) {
      // Scaled down through mipmaps (trilinear), so the lines and the mask
      // average out instead of aliasing and crawling as the picture moves.
      gl.bindTexture(gl.TEXTURE_2D, out.t);
      gl.generateMipmap(gl.TEXTURE_2D);
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
      gl.viewport(fx, this.canvas.height - fy - fh, fw, fh);
      gl.useProgram(this.copy.p);
      gl.uniform1i(this.copy.u.picture, 0);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }
  }

  // An 8-bit target of w x h for the tube, kept while the size holds.
  offscreen(w, h) {
    const gl = this.gl;
    if (this.spare?.w === w && this.spare?.h === h) return this.spare;
    if (this.spare) {
      gl.deleteTexture(this.spare.t);
      gl.deleteFramebuffer(this.spare.fb);
    }
    const t = texture(gl, gl.LINEAR_MIPMAP_LINEAR);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
    const fb = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, t, 0);
    this.spare = { t, fb, w, h };
    return this.spare;
  }
}

export const RASTER = { width: RASTER_W, height: RASTER_H };
