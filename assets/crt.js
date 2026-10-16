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
// 2. The tube, at the canvas's own pixels: the raster on a bowed face (so
//    it bends near the edges, and the set's overscan crops its edges), 480
//    lines drawn by a beam that widens with brightness, an aperture grille,
//    light scattering in the glass round bright parts, the tube's gamma,
//    the corners dimmer than the middle, the power-off collapse to a dot
//    and the purity wobble of the degaussing coil at power-on.
//
// Every effect is worked out in the tube's light (linear), and the lines
// and grille are integrated over each pixel, so the picture doesn't shimmer
// with moire at sizes the lines can't be drawn: then they fade to an even
// glow, as a real tube's do at a distance.

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

// Pass 2: the tube.
const tube = `#version 300 es
precision highp float;
in vec2 uv;
out vec4 color;
uniform sampler2D signal;
uniform vec2 size;       // the canvas, in pixels
uniform vec4 face;       // the tube's face in the canvas, in pixels: x, y, w, h
uniform float radius;    // its corners, in pixels
uniform float power;     // the beam's strength: 1 on, 0 off (and dips between shows)
uniform float collapse;  // the power-off collapse, 0 to 1
uniform float degauss;   // 1 at power-on, fading to 0
uniform float time;
uniform float grille;    // how strong the aperture grille is

const float PI = 3.14159265;

float roundedBox(vec2 p, vec2 extent, float r) {
  vec2 q = abs(p) - extent + r;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

// The fraction of a pixel spanning [a, b] lit by a beam (a Gaussian of
// sigma) centered at 0: the Gaussian integrated over the pixel.
float erf(float x) {
  float t = 1.0 / (1.0 + 0.47047 * abs(x));
  float y = 1.0 - t * (0.3480242 + t * (-0.0958798 + t * 0.7478556)) * exp(-x * x);
  return sign(x) * y;
}
float covered(float a, float b, float sigma) {
  float s = sigma * 1.41421356;
  return 0.5 * (erf(b / s) - erf(a / s)) / max(b - a, 1e-4);
}

vec3 linear(vec3 c) { return pow(c, vec3(2.4)); }  // a tube's gamma

void main() {
  vec2 frag = vec2(uv.x, 1.0 - uv.y) * size;
  vec2 c = (frag - face.xy - face.zw * 0.5) / (face.zw * 0.5);  // -1..1 over the face
  // The face's own color where no beam lands: the phosphor's dark gray,
  // under the room's light.
  vec3 glass = vec3(0.0035, 0.0045, 0.004);

  // The bowed face: the raster bends outward toward the edges, and the
  // set's 5% overscan crops the picture's own edges.
  vec2 bent = c * (1.0 + 0.045 * dot(c, c)) / 1.05;
  // Collapsing: the raster squeezes to a line, then to a dot.
  float squeezeY = mix(1.0, 0.006, smoothstep(0.0, 0.35, collapse));
  float squeezeX = mix(1.0, 0.012, smoothstep(0.35, 0.55, collapse));
  vec2 r = bent / vec2(squeezeX, squeezeY);
  vec2 p = r * 0.5 + 0.5;  // 0..1 over the raster, top first

  vec3 light = vec3(0.0);
  if (all(greaterThanEqual(p, vec2(0.0))) && all(lessThanEqual(p, vec2(1.0)))) {
    // How many raster lines and pixels one canvas pixel spans here.
    vec2 perPixel = vec2(${RASTER_W}.0, ${RASTER_H}.0) / face.zw * 1.05 / vec2(squeezeX, squeezeY);
    float lineAt = p.y * ${RASTER_H}.0 - 0.5;
    float l0 = floor(lineAt + 0.5);
    float reach = perPixel.y * 0.5;
    // The beam from the nearest line and the ones either side, which a
    // wide (bright) beam reaches into.
    for (int k = -1; k <= 1; k++) {
      float l = l0 + float(k);
      if (l < 0.0 || l > ${RASTER_H}.0 - 1.0) continue;
      vec2 at = vec2(p.x, (l + 0.5) / ${RASTER_H}.0);
      vec3 rgb = textureLod(signal, vec2(at.x, 1.0 - at.y), 0.0).rgb;
      vec3 lin = linear(rgb);
      // The beam widens with its current: bright parts fill the gaps
      // between lines. One width for all three guns, from the line's
      // brightness, keeps it to one integral a line.
      float sigma = mix(0.3, 0.52, sqrt(dot(lin, vec3(0.2126, 0.7152, 0.0722))));
      float d = lineAt - l;
      light += lin * covered(d - reach, d + reach, sigma);
    }
    // Halation: light scattered in the thick glass round bright parts.
    vec3 halo = linear(textureLod(signal, vec2(p.x, 1.0 - p.y), 4.0).rgb);
    light += halo * 0.07;
    // The beam lands less squarely, and dimmer, toward the corners.
    light *= 1.0 - 0.28 * pow(dot(c * vec2(0.9, 1.0), c * vec2(0.9, 1.0)) * 0.5, 2.0);
    // Collapsing, the beam's light only ever fades (so the screen never
    // brightens as a whole, which would flash), while the shrinking
    // picture gets brighter for its size, up to a point: a bright line,
    // then a dot.
    float energy = 1.0 - smoothstep(0.05, 1.0, collapse);
    float area = squeezeX * squeezeY;
    light *= min(energy / max(area, 1e-4), 2.5) * power;
  }
  if (collapse > 0.0) {
    // The dot's afterglow on the phosphor, fading with the beam.
    float energy = 1.0 - smoothstep(0.05, 1.0, collapse);
    float d = length(c * vec2(face.z / face.w, 1.0));
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

  // The aperture grille, in the canvas's own pixels: stripes of red, green
  // and blue phosphor three pixels wide, which a scaled pattern couldn't
  // draw without moire. Brightened to make up for the light it takes.
  float column = mod(floor(frag.x), 3.0);
  vec3 stripe = vec3(column == 0.0, column == 1.0, column == 2.0);
  vec3 mask = mix(vec3(1.0), stripe * 3.0, grille / 3.0);
  // A slightly cool white, as sets of the day were set (9300 K).
  light *= mask * vec3(0.97, 0.99, 1.04) * 1.12;

  vec3 lit = glass + light;
  // Out of the tube's light into the display's sRGB.
  vec3 srgb = pow(clamp(lit, 0.0, 1.0), vec3(1.0 / 2.2));
  // The face's rounded outline: dark past it (the set's gasket covers
  // the edge on the page), softened over a pixel.
  float edge = roundedBox(frag - face.xy - face.zw * 0.5, face.zw * 0.5, radius);
  float inside = 1.0 - smoothstep(-1.0, 0.5, edge);
  color = vec4(srgb * inside, 1.0);
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
    this.yiq = target(gl, false);
    this.signal = target(gl, true);
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
  // degauss (0-1), snow (0-1), roll (frames), jitter (px), noise, grille,
  // and face (the tube's face in canvas pixels) and radius.
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
    gl.bindTexture(gl.TEXTURE_2D, this.signal.t);
    gl.generateMipmap(gl.TEXTURE_2D);

    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.useProgram(this.tube.p);
    u = this.tube.u;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.signal.t);
    gl.uniform1i(u.signal, 0);
    gl.uniform2f(u.size, this.canvas.width, this.canvas.height);
    gl.uniform4fv(u.face, s.face);
    gl.uniform1f(u.radius, s.radius);
    gl.uniform1f(u.power, s.power);
    gl.uniform1f(u.collapse, s.collapse || 0);
    gl.uniform1f(u.degauss, s.degauss || 0);
    gl.uniform1f(u.time, time);
    gl.uniform1f(u.grille, s.grille ?? 0.3);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}

export const RASTER = { width: RASTER_W, height: RASTER_H };
