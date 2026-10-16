"""Renders the station's bumpers for /tv: short idents and an "up next"
card, aired between shows (src/tv.rs). Early-90s kids' TV packaging in the
clock radio's toy plastic (radio.py): the orange splat, lime slime and the
radio's lime ball, on purple. Each bumper's splat is drawn from its own seed,
as that era's idents never showed the logo the same way twice.

They're drawn at the broadcast raster, 640 x 480, and at video's 30
frames a second: off-air recordings of the day's packaging show its CGI
moving every frame, with the limited-animation feel coming from holds (a
letter popping on and staying put). The motion is written in ticks, twelfths
of a second, and every bumper's sound (slide whistles, boings, a splat, a
kazoo fanfare) is synthesized here from the same ticks, so the two stay in
sync.

- <name>/0001.png...: each bumper's frames.
- <name>.wav: its sound, 48 kHz mono.
- bumpers.json: each bumper's name, file, length, and for the up-next card
  where (in raster pixels) and when (in seconds) the page writes the next
  show's name on its blank panel.

    blender -b -P assets/blender/bumpers.py -- /tmp/b /path/to/fonts [samples]

`just bumpers` runs it, then encodes each into assets/bumper-<name>.mp4
with its sound brought to -15 LUFS, a loudness unit over the shows'
(loudness.rs's -16), as the day's bumpers ran a little hot; it airs at
gain 0.
"""

import json
import math
import os
import random
import sys
import wave

import bmesh
import bpy
import numpy as np
from bpy_extras.object_utils import world_to_camera_view
from mathutils import Vector

argv = sys.argv[sys.argv.index("--") + 1 :]
OUT = argv[0]
FONTS = argv[1]
SAMPLES = int(argv[2]) if len(argv) > 2 else 64
ONLY = argv[3].split(",") if len(argv) > 3 else None

FPS = 12  # ticks a second: the unit the motion and sound are written in
RENDER_FPS = 30
W, H = 640, 480
RATE = 48000

LUCKIEST = f"{FONTS}/LuckiestGuy.ttf"
FREDOKA = f"{FONTS}/Fredoka.ttf"


def lin(hex_color):
    out = []
    for i in (0, 2, 4):
        c = int(hex_color.lstrip("#")[i : i + 2], 16) / 255
        out.append(c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4)
    return out


# ---- The scene ---------------------------------------------------------------


def fresh():
    """An empty scene with the camera, light and render settings every
    bumper shares. The camera looks along +y at a 14.4 x 10.8 frame at y=0."""
    bpy.ops.wm.read_factory_settings(use_empty=True)
    scene = bpy.context.scene
    scene.render.engine = "CYCLES"
    prefs = bpy.context.preferences.addons["cycles"].preferences
    for kind in ("OPTIX", "CUDA"):
        try:
            prefs.compute_device_type = kind
            prefs.get_devices()
            if any(d.type == kind for d in prefs.devices):
                for d in prefs.devices:
                    d.use = d.type == kind
                scene.cycles.device = "GPU"
                break
        except TypeError:
            continue
    scene.cycles.samples = SAMPLES
    scene.cycles.use_denoising = True
    scene.cycles.max_bounces = 8
    scene.render.resolution_x = W
    scene.render.resolution_y = H
    scene.render.resolution_percentage = 100
    scene.render.image_settings.file_format = "PNG"
    scene.render.image_settings.color_mode = "RGB"
    scene.view_settings.view_transform = "Standard"
    scene.view_settings.exposure = -0.1

    cam_data = bpy.data.cameras.new("cam")
    cam_data.lens = 50
    cam_data.sensor_fit = "HORIZONTAL"
    cam_data.sensor_width = 36
    cam = bpy.data.objects.new("cam", cam_data)
    scene.collection.objects.link(cam)
    cam.location = (0, -20, 0)
    cam.rotation_euler = (math.radians(90), 0, 0)
    scene.camera = cam

    world = bpy.data.worlds.new("world")
    scene.world = world
    try:
        world.use_nodes = True
    except AttributeError:
        pass
    import glob

    env = world.node_tree.nodes.new("ShaderNodeTexEnvironment")
    env.image = bpy.data.images.load(glob.glob("/usr/share/blender/*/datafiles/studiolights/world/studio.exr")[0])
    bg = world.node_tree.nodes["Background"]
    bg.inputs["Strength"].default_value = 0.22
    world.node_tree.links.new(env.outputs["Color"], bg.inputs["Color"])

    def area(name, loc, power, size, color=(1, 1, 1)):
        data = bpy.data.lights.new(name, "AREA")
        data.energy = power
        data.size = size
        data.color = color
        ob = bpy.data.objects.new(name, data)
        scene.collection.objects.link(ob)
        ob.location = loc
        ob.rotation_euler = (Vector((0, 0, 0)) - Vector(loc)).to_track_quat("-Z", "Y").to_euler()

    # radio.py's key, fill and violet kicker, at this scene's scale.
    area("key", (-12, -16, 12), 6500, 10)
    area("fill", (12, -14, 2), 3500, 10, (0.9, 0.95, 1.0))
    area("rim", (6, 6, 12), 6000, 6, (1.0, 0.97, 0.92))
    area("violet", (-8, 8, 6), 5000, 6, (0.55, 0.3, 1.0))
    return scene, cam


def material(name, color, rough=0.3, coat=0.4, sss=0.1, emit=0.0):
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    p = m.node_tree.nodes["Principled BSDF"]
    p.inputs["Base Color"].default_value = (*lin(color), 1)
    p.inputs["Roughness"].default_value = rough
    p.inputs["Coat Weight"].default_value = coat
    p.inputs["Coat Roughness"].default_value = 0.06
    if sss:
        p.inputs["Subsurface Weight"].default_value = sss
        p.inputs["Subsurface Radius"].default_value = (0.6, 0.6, 0.6)
        p.inputs["Subsurface Scale"].default_value = 0.1
    if emit:
        p.inputs["Emission Color"].default_value = (*lin(color), 1)
        p.inputs["Emission Strength"].default_value = emit
    return m


def goo(m):
    """Slime's lumpy, wet surface: a coarse bump, so it catches light in
    blobs rather than mirroring the studio flat."""
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    n = nt.nodes.new("ShaderNodeTexNoise")
    n.inputs["Scale"].default_value = 0.9
    n.inputs["Detail"].default_value = 3
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = 0.45
    nt.links.new(n.outputs["Fac"], bump.inputs["Height"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Coat Normal"])
    return m


def palette():
    return {
        "orange": material("orange", "#ff7a00", rough=0.32, coat=0.35, sss=0.15),
        "orange-dark": material("orange-dark", "#c94f00", rough=0.3),
        "white": material("white", "#fbfbf4", rough=0.35, coat=0.3),
        "ink": material("ink", "#1d0f3f", rough=0.5),
        "purple": material("purple", "#5b2fc4", rough=0.3, coat=0.4),
        "purple-dark": material("purple-dark", "#2b1660", rough=0.45, coat=0.1),
        "violet": material("violet", "#8f5cf5", rough=0.3, coat=0.4),
        "lime": material("lime", "#8be01a", rough=0.25, coat=0.5, sss=0.2),
        "slime": goo(material("slime", "#7fd816", rough=0.32, coat=0.5, sss=0.5)),
        "ball": material("ball", "#82dc1c", rough=0.14, coat=0.7, sss=0.3),
        "red": material("red", "#e3231a", rough=0.12, coat=0.8, sss=0.35),
        # The day's endcards set their type on flat purple panels.
        "panel": material("panel", "#4a1386", rough=0.7, coat=0.0, sss=0.0),
    }


def link(ob):
    bpy.context.scene.collection.objects.link(ob)
    return ob


def mesh_from(name, bm, mat, smooth=True):
    me = bpy.data.meshes.new(name)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    bm.to_mesh(me)
    bm.free()
    me.materials.append(mat)
    for poly in me.polygons:
        poly.use_smooth = smooth
    return me


def slab(name, pts, depth, mat):
    """A front-view outline (x, z) extruded back along +y by `depth`."""
    bm = bmesh.new()
    front = [bm.verts.new((x, 0, z)) for x, z in pts]
    back = [bm.verts.new((x, depth, z)) for x, z in pts]
    n = len(pts)
    bm.faces.new(front)
    bm.faces.new(list(reversed(back)))
    for i in range(n):
        j = (i + 1) % n
        bm.faces.new((front[i], front[j], back[j], back[i]))
    return mesh_from(name, bm, mat)


def bevelled(ob, width, segments=3):
    mod = ob.modifiers.new("bevel", "BEVEL")
    mod.width = width
    mod.segments = segments
    mod.limit_method = "ANGLE"
    mod.harden_normals = True
    return ob


def rrect(w, h, r, seg=10):
    pts = []
    for cx, cz, a0 in ((w / 2 - r, h / 2 - r, 0), (-w / 2 + r, h / 2 - r, 90), (-w / 2 + r, -h / 2 + r, 180), (w / 2 - r, -h / 2 + r, 270)):
        for i in range(seg + 1):
            a = math.radians(a0 + 90 * i / seg)
            pts.append((cx + r * math.cos(a), cz + r * math.sin(a)))
    return pts


def splat_outline(seed, rx, rz, phase=0.0, n=160):
    """A splat: an ellipse with lobes and a few long splashes, its shape
    drawn from `seed`. `phase` wobbles the lobes, for a splat that's still
    settling."""
    rng = random.Random(seed)
    lobes = [(k, rng.uniform(0.03, 0.09) / (1 + k * 0.15), rng.uniform(0, 6.28)) for k in range(3, 11)]
    splashes = [(rng.uniform(0, 6.28), rng.uniform(0.25, 0.5), rng.uniform(0.07, 0.13)) for _ in range(rng.randint(4, 6))]
    pts = []
    for i in range(n):
        t = 2 * math.pi * i / n
        r = 1.0
        for k, a, p in lobes:
            r += a * math.sin(k * t + p + phase * (0.6 + 0.1 * k))
        for at, length, width in splashes:
            d = math.atan2(math.sin(t - at), math.cos(t - at))
            r += length * math.exp(-(d / width) ** 2)
        pts.append((rx * r * math.cos(t), rz * r * math.sin(t)))
    return pts


def letters(word, font, size, mat, edge, spacing=0.0, depth=0.12):
    """`word` as one text object per letter, laid out as the font sets it,
    each with an ink copy behind it for an outline. Returns a list of
    (letter, edge, x) with x each letter's center from the word's."""
    out = []
    widths = []
    for ch in word:
        cu = bpy.data.curves.new("measure", "FONT")
        cu.body = ch
        cu.font = bpy.data.fonts.load(font, check_existing=True)
        cu.size = size
        ob = link(bpy.data.objects.new("measure", cu))
        bpy.context.view_layer.update()
        widths.append(ob.dimensions.x if ch != " " else size * 0.35)
        bpy.data.objects.remove(ob)
    total = sum(widths) + spacing * (len(word) - 1)
    x = -total / 2
    for ch, w_ in zip(word, widths):
        cx = x + w_ / 2
        x += w_ + spacing
        if ch == " ":
            continue
        made = []
        for name, m, offset, extrude, y in ((ch, mat, 0.0, depth, 0.0), (ch + "-edge", edge, size * 0.07, depth * 0.8, depth * 0.6)):
            cu = bpy.data.curves.new(name, "FONT")
            cu.body = ch
            cu.font = bpy.data.fonts.load(font, check_existing=True)
            cu.size = size
            cu.extrude = extrude
            cu.offset = offset
            cu.bevel_depth = 0.02 if not offset else 0.0
            cu.align_x = "CENTER"
            cu.align_y = "CENTER"
            ob = link(bpy.data.objects.new(name, cu))
            ob.rotation_euler = (math.radians(90), 0, 0)
            ob.data.materials.append(m)
            ob["y"] = y
            made.append(ob)
        out.append((made[0], made[1], cx))
    return out


def place_letter(letter, x, z, y, scale, rot=0.0):
    for ob in letter[:2]:
        ob.location = (x, y + ob["y"], z)
        ob.scale = (scale, scale, scale)
        ob.rotation_euler = (math.radians(90), math.radians(rot), 0)


def sphere(name, r, mat):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=48, ring_count=24, radius=r)
    ob = bpy.context.active_object
    ob.name = name
    for poly in ob.data.polygons:
        poly.use_smooth = True
    ob.data.materials.append(mat)
    return ob


def backdrop(mat, y=4.0, size=60):
    bpy.ops.mesh.primitive_plane_add(size=size, location=(0, y, 0), rotation=(math.radians(90), 0, 0))
    ob = bpy.context.active_object
    ob.data.materials.append(mat)
    return ob


def rays_material(name, a, b, count=16):
    """A backdrop of rays from the middle, alternating `a` and `b`, which
    turn as `turn` (a value the frames set) goes up: a 90s promo's
    sunburst."""
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    p.inputs["Roughness"].default_value = 0.6
    tc = nt.nodes.new("ShaderNodeTexCoord")
    sep = nt.nodes.new("ShaderNodeSeparateXYZ")
    nt.links.new(tc.outputs["Object"], sep.inputs["Vector"])
    ang = nt.nodes.new("ShaderNodeMath")
    ang.operation = "ARCTAN2"
    nt.links.new(sep.outputs["Y"], ang.inputs[0])
    nt.links.new(sep.outputs["X"], ang.inputs[1])
    scale = nt.nodes.new("ShaderNodeMath")
    scale.operation = "MULTIPLY"
    scale.inputs[1].default_value = count / (2 * math.pi)
    nt.links.new(ang.outputs[0], scale.inputs[0])
    turn = nt.nodes.new("ShaderNodeValue")
    turn.name = "turn"
    add = nt.nodes.new("ShaderNodeMath")
    add.operation = "ADD"
    nt.links.new(scale.outputs[0], add.inputs[0])
    nt.links.new(turn.outputs[0], add.inputs[1])
    frac = nt.nodes.new("ShaderNodeMath")
    frac.operation = "FRACT"
    nt.links.new(add.outputs[0], frac.inputs[0])
    step = nt.nodes.new("ShaderNodeMath")
    step.operation = "GREATER_THAN"
    step.inputs[1].default_value = 0.5
    nt.links.new(frac.outputs[0], step.inputs[0])
    mix = nt.nodes.new("ShaderNodeMix")
    mix.data_type = "RGBA"
    mix.inputs["A"].default_value = (*lin(a), 1)
    mix.inputs["B"].default_value = (*lin(b), 1)
    nt.links.new(step.outputs[0], mix.inputs["Factor"])
    nt.links.new(mix.outputs["Result"], p.inputs["Base Color"])
    return m


def overshoot(t):
    """0 to 1 with a bounce past 1 and back: a pop-in's scale, on twos."""
    if t <= 0:
        return 0.0
    if t >= 1:
        return 1.0
    return 1 - math.cos(t * math.pi * 1.5) * math.exp(-4 * t) if t > 0 else 0.0


def ease_out(t):
    t = min(max(t, 0.0), 1.0)
    return 1 - (1 - t) ** 3


def to_raster(cam, point):
    c = world_to_camera_view(bpy.context.scene, cam, Vector(point))
    return (c.x * W, (1 - c.y) * H)


# ---- Sound -----------------------------------------------------------------------


def silence(seconds):
    return np.zeros(int(seconds * RATE), dtype=np.float64)


def env(n, attack=0.005, release=0.05):
    t = np.arange(n) / RATE
    a = np.clip(t / max(attack, 1e-4), 0, 1)
    r = np.clip((n / RATE - t) / max(release, 1e-4), 0, 1)
    return a * r


def tone(freq, seconds, shape="sine", harmonics=8):
    """An oscillator following `freq` (a number or an array a sample),
    band-limited: a sine, or a saw or square summed from harmonics."""
    n = int(seconds * RATE)
    f = np.broadcast_to(np.asarray(freq, dtype=np.float64), (n,)) if np.ndim(freq) == 0 else np.asarray(freq)[:n]
    phase = 2 * np.pi * np.cumsum(f) / RATE
    if shape == "sine":
        return np.sin(phase)
    out = np.zeros(n)
    for k in range(1, harmonics + 1):
        if shape == "square" and k % 2 == 0:
            continue
        amp = 1 / k
        # Leave out harmonics past the Nyquist frequency.
        out += np.where(f * k < RATE / 2, amp * np.sin(k * phase), 0)
    return out * (0.6 if shape == "saw" else 0.8)


def lowpass(x, cutoff):
    """A one-pole low-pass, its cutoff (Hz) a number or an array a sample."""
    c = np.broadcast_to(np.asarray(cutoff, dtype=np.float64), x.shape)
    a = 1 - np.exp(-2 * np.pi * c / RATE)
    y = np.empty_like(x)
    acc = 0.0
    for i in range(len(x)):
        acc += a[i] * (x[i] - acc)
        y[i] = acc
    return y


def bandpass(x, low, high):
    return lowpass(x, high) - lowpass(lowpass(x, high), low)


def noise(seconds, seed=0):
    return np.random.default_rng(seed).uniform(-1, 1, int(seconds * RATE))


def mix(track, at, sound, gain=1.0):
    start = int(at * RATE)
    end = min(len(track), start + len(sound))
    if end > start:
        track[start:end] += sound[: end - start] * gain


def slide_whistle(f0, f1, seconds, seed=1):
    """A slide whistle: a pure tone gliding (exponentially) from f0 to f1,
    with a player's vibrato and breath."""
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    glide = f0 * (f1 / f0) ** (t / seconds)
    freq = glide * (1 + 0.012 * np.sin(2 * np.pi * 6 * t))
    body = tone(freq, seconds) + 0.18 * tone(freq * 2, seconds)
    breath = bandpass(noise(seconds, seed), 800, 5000) * 0.25
    return (body + breath) * env(n, 0.03, 0.08) * 0.5


def boing(freq, seconds=0.45):
    """A spring's boing: a twang whose pitch wobbles and settles."""
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    wobble = 1 + 0.35 * np.exp(-t * 7) * np.sin(2 * np.pi * 14 * t)
    f = freq * wobble * (1 + 0.6 * np.exp(-t * 30))
    body = tone(f, seconds) + 0.3 * tone(f * 2.01, seconds) + 0.15 * tone(f * 3.03, seconds)
    return body * np.exp(-t * 6) * env(n, 0.002, 0.05) * 0.55


def splat_sound(seed=3):
    """A wet splat: a thump, a burst of squelchy noise, and a squelch
    falling in pitch."""
    seconds = 0.6
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    thump = tone(110 * np.exp(-t * 9) + 45, seconds) * np.exp(-t * 14)
    burst = lowpass(noise(seconds, seed), 2600 * np.exp(-t * 6) + 200) * np.exp(-t * 9) * 2.5
    squelch = tone(520 * np.exp(-t * 7) + 90, seconds, "square", 6) * np.exp(-t * 11) * 0.35
    return (thump * 0.9 + burst + squelch) * env(n, 0.001, 0.1) * 0.7


def bloop(freq, seconds=0.12, up=True):
    """A bubble's pop: a quick pitch sweep."""
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    sweep = (1 + 2.2 * t / seconds) if up else (2.4 - 1.6 * t / seconds)
    return tone(freq * sweep, seconds) * np.exp(-t * 18) * env(n, 0.002, 0.02) * 0.5


def kazoo(freqs, seconds):
    """A kazoo chord: buzzy saws through a nasal formant."""
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    out = np.zeros(n)
    for f in freqs:
        out += tone(f * (1 + 0.006 * np.sin(2 * np.pi * 5.5 * t)), seconds, "saw", 24)
    out = bandpass(out, 500, 3200) * 1.6
    return out / max(1, len(freqs)) * env(n, 0.015, 0.06) * 0.8


def marimba(freq, seconds=0.35):
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    body = tone(freq, seconds) * np.exp(-t * 9) + 0.35 * tone(freq * 4, seconds) * np.exp(-t * 30)
    return body * env(n, 0.001, 0.04) * 0.5


def kick(seconds=0.25):
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    return tone(55 + 110 * np.exp(-t * 30), seconds) * np.exp(-t * 12) * 0.9


def snare(seconds=0.2, seed=5):
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    rattle = bandpass(noise(seconds, seed), 1200, 7000) * np.exp(-t * 22)
    body = tone(190, seconds) * np.exp(-t * 30)
    return (rattle * 0.9 + body * 0.5) * 0.7


def hat(seconds=0.06, seed=7):
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    return (noise(seconds, seed) - lowpass(noise(seconds, seed), 7000)) * np.exp(-t * 60) * 0.4


def whoosh(seconds, rising=True, seed=9):
    n = int(seconds * RATE)
    t = np.arange(n) / RATE
    sweep = np.linspace(300, 4000, n) if rising else np.linspace(4000, 300, n)
    out = bandpass(noise(seconds, seed), sweep * 0.6, sweep * 1.4)
    return out * np.sin(np.pi * t / seconds) ** 2 * 1.2


def glug(seconds, seed=11):
    """Slime pouring: a run of low bubbles."""
    out = silence(seconds)
    rng = random.Random(seed)
    at = 0.0
    while at < seconds - 0.1:
        mix(out, at, bloop(rng.uniform(140, 260), rng.uniform(0.08, 0.14)), rng.uniform(0.5, 1.0))
        at += rng.uniform(0.05, 0.11)
    return lowpass(out, 1800) * 1.4


NOTE = {name: 440 * 2 ** ((k - 9) / 12) for k, name in enumerate("C C# D D# E F F# G G# A A# B".split())}


def note(name, octave):
    return NOTE[name] * 2 ** (octave - 4)


def write_wav(path, track):
    peak = np.max(np.abs(track)) or 1.0
    data = np.clip(track / peak * 0.9, -1, 1)
    with wave.open(path, "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(RATE)
        f.writeframes((data * 32767).astype("<i2").tobytes())


# ---- Rendering --------------------------------------------------------------------


def render_frames(name, ticks, update):
    """Render `ticks` of motion at RENDER_FPS, `update` posing the scene at
    each frame's time in ticks."""
    scene = bpy.context.scene
    folder = os.path.join(OUT, name)
    os.makedirs(folder, exist_ok=True)
    for f in range(round(ticks * RENDER_FPS / FPS)):
        update(f * FPS / RENDER_FPS)
        scene.render.filepath = os.path.join(folder, f"{f + 1:04d}.png")
        bpy.ops.render.render(write_still=True)


# ---- Bumper: splat ------------------------------------------------------------------
# An orange blob flies in on a slide whistle and splats onto the screen,
# the wordmark pops onto it a letter at a time, a kazoo fanfare, then it all
# spins away.


def bumper_splat():
    scene, cam = fresh()
    m = palette()
    backdrop(rays_material("rays", "#3a1d86", "#2b1660", 18))
    rays = bpy.data.materials["rays"].node_tree.nodes["turn"]
    seed = 1994

    blob = sphere("blob", 1.0, m["orange"])
    splat = link(bpy.data.objects.new("splat", slab("splat", splat_outline(seed, 5.6, 2.5), 0.5, m["orange"])))
    bevelled(splat, 0.18)
    shadow = link(bpy.data.objects.new("splat-shadow", slab("splat-shadow", splat_outline(seed, 5.6, 2.5), 0.1, m["ink"])))
    word = letters("JOYSCROLLER", LUCKIEST, 2.05, m["white"], m["ink"], spacing=0.02)
    rng = random.Random(seed)
    tilt = [rng.uniform(-9, 9) for _ in word]
    drops = []
    for k in range(9):
        d = sphere(f"drop{k}", rng.uniform(0.13, 0.3), m["orange"])
        a = rng.uniform(0, 2 * math.pi)
        drops.append((d, a, rng.uniform(5.2, 7.5)))

    FRAMES = 54
    HIT = 10

    def update(f):
        rays.outputs[0].default_value = f * 0.035
        # Flying in on an arc from the bottom left, stretched along the way
        # it moves, turning.
        if f < HIT:
            t = f / HIT
            x, z = -9 + 9 * t, -7 + 7 * (2 * t - t * t)
            blob.hide_render = False
            blob.location = (x, -1.0, z)
            stretch = 1.0 + 0.6 * t
            blob.scale = (stretch, 1.0, 1 / stretch)
            blob.rotation_euler = (0, -math.atan2(7 * (2 - 2 * t), 9), 0)
        else:
            blob.hide_render = True
        # The splat lands and settles with a wobble.
        if f >= HIT:
            settle = [0.5, 1.18, 0.94, 1.04, 0.99]
            s = settle[int(f - HIT)] if f - HIT < len(settle) else 1.0
            wob = max(0.0, 1 - (f - HIT) / 30)
            outline = splat_outline(seed, 5.6, 2.5, phase=(f - HIT) * 0.9 * wob)
            splat.data = slab("splat", outline, 0.5, m["orange"])
            shadow.data = slab("splat-shadow", outline, 0.1, m["ink"])
            for ob in (splat, shadow):
                ob.hide_render = False
            spin = 0.0
            out = 1.0
            if f >= 47:
                # Off it goes: a spin down to nothing.
                k = (f - 47) / 6
                out = max(0.0, 1 - k) ** 1.5
                spin = k * 200
            splat.scale = (s * out, 1, s * out)
            shadow.scale = (s * out * 1.03, 1, s * out * 1.03)
            splat.location = (0, 0, 0.2)
            shadow.location = (0.22, 0.45, 0.0)
            for ob in (splat, shadow):
                ob.rotation_euler = (0, math.radians(-6 + spin), 0)
        else:
            splat.hide_render = shadow.hide_render = True
        # Letters pop on, one a frame, and bob on twos.
        for k, (letter, edge, cx) in enumerate(word):
            t = (f - (HIT + 4 + k)) / 3
            s = overshoot(t)
            if f >= 47:
                s *= max(0.0, 1 - (f - 47) / 6) ** 1.5
            bob = 0.12 * math.sin(f * 1.3 + k * 0.9) if f > HIT + 16 else 0.0
            vis = s > 0.01
            letter.hide_render = edge.hide_render = not vis
            ang = math.radians(-6 + (0 if f < 47 else (f - 47) / 6 * 200))
            x = cx * math.cos(ang) * (1 if f < 47 else max(0.0, 1 - (f - 47) / 6) ** 1.5)
            z = 0.25 + bob - cx * math.sin(ang) * (1 if f < 47 else max(0.0, 1 - (f - 47) / 6) ** 1.5)
            place_letter((letter, edge), x, z, -0.75, s, tilt[k] - 6)
        # Droplets fly out from the hit and stick.
        for d, a, r in drops:
            if f < HIT:
                d.hide_render = True
                continue
            t = ease_out((f - HIT) / 4)
            k = max(0.0, 1 - (f - 47) / 6) ** 1.5 if f >= 47 else 1.0
            d.hide_render = k < 0.02
            d.location = (math.cos(a) * r * 0.95 * t * k, -0.3, math.sin(a) * r * 0.6 * t * k + 0.2)
            d.scale = (1.2, 0.5, 1.2) if t >= 1 else (1, 1, 1)

    render_frames("splat", FRAMES, update)

    seconds = FRAMES / FPS
    track = silence(seconds + 0.2)
    mix(track, 0.0, slide_whistle(380, 1500, HIT / FPS))
    mix(track, HIT / FPS, splat_sound(), 1.2)
    scale = ["C", "D", "E", "G", "A"]
    for k in range(11):
        mix(track, (HIT + 4 + k) / FPS, bloop(note(scale[k % 5], 5 + k // 5) * 0.5), 0.6)
    fanfare = HIT + 18
    mix(track, fanfare / FPS, kazoo([note("C", 4), note("E", 4), note("G", 4)], 0.16), 0.9)
    mix(track, (fanfare + 2) / FPS, kazoo([note("C", 4), note("F", 4), note("A", 4)], 0.16), 0.9)
    mix(track, (fanfare + 4) / FPS, kazoo([note("C", 4), note("E", 4), note("G", 4), note("C", 5)], 0.8), 1.0)
    mix(track, (fanfare + 4) / FPS, kick(), 0.8)
    mix(track, 47 / FPS, slide_whistle(1400, 300, 6 / FPS, seed=2), 0.9)
    mix(track, 53 / FPS, bloop(300, 0.15, up=False), 0.8)
    write_wav(os.path.join(OUT, "splat.wav"), track)
    return {"name": "splat", "file": "bumper-splat.mp4", "seconds": round(seconds, 3)}


# ---- Bumper: up next ------------------------------------------------------------------
# Slime pours down over the screen, an UP NEXT! sign bounces in on it, and
# holds still while the page writes the next show's name on its panel; then
# it drops away and the slime drains.


def bumper_up_next():
    scene, cam = fresh()
    m = palette()
    backdrop(m["purple-dark"])
    seed = 1991
    rng = random.Random(seed)

    # The slime: a sheet with a dripping lower edge, sliding down.
    def sheet(drop):
        pts = []
        n = 90
        for i in range(n + 1):
            x = -9 + 18 * i / n
            z = drop
            for k, (at, depth, width) in enumerate(drips):
                z -= depth * math.exp(-(((x - at) / width) ** 2))
            z += 0.18 * math.sin(x * 2.1 + drop)
            pts.append((x, z))
        pts += [(9, 9), (-9, 9)]
        return pts

    drips = [(rng.uniform(-8, 8), rng.uniform(0.4, 1.6), rng.uniform(0.2, 0.45)) for _ in range(12)]
    slime = link(bpy.data.objects.new("slime", slab("slime", sheet(6), 0.4, m["slime"])))
    bevelled(slime, 0.15)
    slime.location = (0, 2.5, 0)

    # The sign: a purple slab with a lime panel and UP NEXT! over it.
    SIGN_W, SIGN_H = 11.2, 6.6
    sign = link(bpy.data.objects.new("sign", slab("sign", rrect(SIGN_W, SIGN_H, 1.1), 0.6, m["orange"])))
    bevelled(sign, 0.18)
    rim = link(bpy.data.objects.new("sign-rim", slab("sign-rim", rrect(SIGN_W + 0.5, SIGN_H + 0.5, 1.3), 0.4, m["purple"])))
    bevelled(rim, 0.12)
    PANEL_W, PANEL_H, PANEL_Z = 9.6, 2.9, -1.05
    panel = link(bpy.data.objects.new("panel", slab("panel", rrect(PANEL_W, PANEL_H, 0.4), 0.1, m["panel"])))
    words = letters("UP NEXT!", LUCKIEST, 1.9, m["white"], m["ink"], spacing=0.05)
    tilts = [rng.uniform(-7, 7) for _ in words]
    parts = [sign, rim, panel]

    FRAMES = 78
    POUR = 12
    IN0, IN1 = 13, 24
    OUT0 = 66
    # The sign's resting place, from which the frames offset it.
    REST = Vector((0, -1.0, 0.1))

    def sign_at(f):
        if f < IN0:
            return None
        if f < IN1:
            t = (f - IN0) / (IN1 - IN0)
            x = 15 * (1 - overshoot(t * 1.05))
            return REST + Vector((x, 0, 0)), 8 * math.sin(t * 9) * (1 - t)
        if f < OUT0:
            return REST, 0.0
        t = (f - OUT0) / 6
        return REST + Vector((0, 0, -14 * t * t)), 25 * t

    def update(f):
        # Slime: pours down to cover the frame, holds (dripping a little
        # further), and slides off at the end.
        if f < POUR:
            drop = 6.5 - 13 * ease_out(f / POUR)
        elif f < OUT0:
            drop = -6.5 - 0.02 * (f - POUR)
        else:
            drop = -6.5 - 2.4 * (f - OUT0)
        slime.data = slab("slime", sheet(drop + 0.0), 0.4, m["slime"])
        # Drips lengthen as it hangs.
        at = sign_at(f)
        for ob in parts:
            ob.hide_render = at is None
        for letter, edge, _ in words:
            letter.hide_render = edge.hide_render = at is None
        if at is None:
            return
        pos, rot = at
        sign.location = pos
        rim.location = pos + Vector((0, 0.3, 0))
        panel.location = pos + Vector((0, -0.08, PANEL_Z))
        for ob in parts:
            ob.rotation_euler = (0, math.radians(rot), 0)
        for k, (letter, edge, cx) in enumerate(words):
            hop = 0.15 * math.sin(f * 1.4 + k) if IN1 <= f < OUT0 else 0.0
            place_letter((letter, edge), pos.x + cx, pos.z + 1.55 + hop, pos.y - 0.2, 1.0, tilts[k] + rot)

    render_frames("up-next", FRAMES, update)

    # Where the panel is, in raster pixels, while the sign holds still.
    sign_at(IN1)
    tl = to_raster(cam, REST + Vector((-PANEL_W / 2 + 0.35, -0.2, PANEL_Z + PANEL_H / 2 - 0.2)))
    br = to_raster(cam, REST + Vector((PANEL_W / 2 - 0.35, -0.2, PANEL_Z - PANEL_H / 2 + 0.2)))

    seconds = FRAMES / FPS
    track = silence(seconds + 0.2)
    mix(track, 0.0, glug(POUR / FPS + 0.2), 0.9)
    mix(track, IN0 / FPS, whoosh(0.4), 0.7)
    mix(track, (IN0 + 6) / FPS, boing(196), 1.0)
    # A drum fill and a marimba run into the hold, then a bouncy two bars
    # under it.
    for k, at in enumerate((24, 25, 26)):
        mix(track, at / FPS, snare(), 0.6 + 0.1 * k)
    mix(track, 27 / FPS, kick(), 1.0)
    run = [("C", 5), ("E", 5), ("G", 5), ("C", 6)]
    for k, (n_, o) in enumerate(run):
        mix(track, (24 + k) / FPS, marimba(note(n_, o)), 0.7)
    beat = 0.25
    bass = [("C", 2), ("C", 3), ("G", 2), ("A#", 2), ("A", 2), ("A", 3), ("F", 2), ("G", 2)]
    tune = [("E", 5), None, ("G", 5), ("E", 5), ("D", 5), None, ("C", 5), None]
    start = 28 / FPS
    k = 0
    while start + k * beat < OUT0 / FPS - 0.1:
        at = start + k * beat
        n_, o = bass[k % len(bass)]
        mix(track, at, tone(note(n_, o), 0.22, "square", 10) * np.exp(-np.arange(int(0.22 * RATE)) / RATE * 8) * 0.35, 1.0)
        if tune[k % len(tune)]:
            mix(track, at, marimba(note(*tune[k % len(tune)])), 0.55)
        if k % 2 == 0:
            mix(track, at, kick(), 0.55)
        else:
            mix(track, at, snare(), 0.35)
        mix(track, at + beat / 2, hat(), 0.5)
        k += 1
    mix(track, OUT0 / FPS, slide_whistle(1200, 250, 0.5, seed=4), 0.9)
    mix(track, (OUT0 + 4) / FPS, glug(0.8, seed=13), 0.7)
    write_wav(os.path.join(OUT, "up-next.wav"), track)
    return {
        "name": "up-next",
        "file": "bumper-up-next.mp4",
        "seconds": round(seconds, 3),
        "text": {
            "box": [round(tl[0]), round(tl[1]), round(br[0] - tl[0]), round(br[1] - tl[1])],
            "from": round((IN1 + 1) / FPS, 3),
            "to": round(OUT0 / FPS, 3),
        },
    }


# ---- Bumper: bounce ---------------------------------------------------------------------
# The radio's lime ball bounces in over a checkered floor; each landing
# springs up a letter, J, O, Y, then TV! pops in over them with confetti.


def bumper_bounce():
    scene, cam = fresh()
    m = palette()
    # Looking down at the floor a little.
    cam.location = (0, -20, 3.2)
    cam.rotation_euler = (math.radians(81), 0, 0)
    backdrop(material("wall", "#5fb812", rough=0.7, coat=0.0, sss=0.0), y=10)
    bpy.ops.mesh.primitive_plane_add(size=60, location=(0, 0, -3.2))
    floor = bpy.context.active_object
    check = bpy.data.materials.new("check")
    try:
        check.use_nodes = True
    except AttributeError:
        pass
    nt = check.node_tree
    p = nt.nodes["Principled BSDF"]
    p.inputs["Roughness"].default_value = 0.45
    p.inputs["Coat Weight"].default_value = 0.15
    ct = nt.nodes.new("ShaderNodeTexChecker")
    ct.inputs["Color1"].default_value = (*lin("#5b2fc4"), 1)
    ct.inputs["Color2"].default_value = (*lin("#8f5cf5"), 1)
    ct.inputs["Scale"].default_value = 18
    nt.links.new(ct.outputs["Color"], p.inputs["Base Color"])
    floor.data.materials.append(check)

    ball = sphere("ball", 0.9, m["ball"])
    rng = random.Random(1993)
    joy = letters("JOY", LUCKIEST, 3.2, m["orange"], m["ink"], spacing=0.5, depth=0.5)
    tv = letters("TV!", LUCKIEST, 2.6, m["red"], m["ink"], spacing=0.2, depth=0.4)
    confetti = []
    colors = [m["orange"], m["lime"], m["violet"], m["white"], m["red"]]
    for k in range(40):
        bpy.ops.mesh.primitive_cube_add(size=0.34)
        c = bpy.context.active_object
        c.scale = (1, 0.2, 0.6)
        c.data.materials.append(rng.choice(colors))
        confetti.append((c, rng.uniform(-6, 6), rng.uniform(3.5, 6.5), rng.uniform(-1, 1), rng.uniform(0, 6.28)))

    FRAMES = 54
    LANDS = [6, 13, 20]
    XS = [joy[0][2], joy[1][2], joy[2][2]]
    FLOOR = -3.2 + 0.9
    TV_AT = 27
    LEAVE = 47

    def ball_at(f):
        """The ball's path: in from the top left, three bounces, then up
        and away to the right."""
        pts = [(-9, 4.5, -2)] + [(x, FLOOR, l) for x, l in zip(XS, LANDS)] + [(9, 5, 28)]
        for (x0, z0, f0), (x1, z1, f1) in zip(pts, pts[1:]):
            if f0 <= f <= f1:
                t = (f - f0) / (f1 - f0)
                x = x0 + (x1 - x0) * t
                peak = max(z0, z1) + 2.2
                z = (1 - t) ** 2 * z0 + 2 * (1 - t) * t * peak + t * t * z1
                return x, z, t
        return None

    def update(f):
        at = ball_at(f)
        ball.hide_render = at is None
        if at:
            x, z, t = at
            ball.location = (x, 0, z)
            squash = 1.0
            if any(abs(f - land) < 0.5 for land in LANDS):
                squash = 0.6
            elif any(abs(f - land) < 1.5 for land in LANDS):
                squash = 1.25
            ball.scale = (1 / math.sqrt(squash), 1 / math.sqrt(squash), squash)
            ball.location.z = z - (0.9 * (1 - squash) if squash < 1 else 0)
        leave = max(0.0, (f - LEAVE) / 6)
        for k, (letter, edge, cx) in enumerate(joy):
            t = (f - LANDS[k] - 0.5) / 3
            s = overshoot(t)
            beat = 0.15 * math.sin((f - TV_AT) * 1.6 + k) if f > TV_AT + 4 else 0.0
            drop = -9 * leave * leave
            letter.hide_render = edge.hide_render = s < 0.01
            for ob in (letter, edge):
                ob.location = (cx, 1.0 + ob["y"], -3.2 + 1.6 * s + beat + drop)
                ob.scale = (s, s, s)
                ob.rotation_euler = (math.radians(90), math.radians((k - 1) * 6), 0)
        for k, (letter, edge, cx) in enumerate(tv):
            t = (f - TV_AT - k) / 3
            s = overshoot(t)
            drop = -12 * leave * leave
            letter.hide_render = edge.hide_render = s < 0.01
            for ob in (letter, edge):
                ob.location = (cx * 1.1, -0.6 + ob["y"], 2.3 + drop + 0.1 * math.sin(f * 1.3 + k))
                ob.scale = (s, s, s)
                ob.rotation_euler = (math.radians(90), math.radians(-8 + k * 5), 0)
        for c, x, height, vx, spin in confetti:
            t = (f - TV_AT) / FPS
            c.hide_render = t < 0
            if t >= 0:
                c.location = (x + vx * t, -2.0, height + 4 * t - 6 * t * t)
                c.rotation_euler = (spin + t * 9, spin * 2 + t * 7, spin)

    render_frames("bounce", FRAMES, update)

    seconds = FRAMES / FPS
    track = silence(seconds + 0.2)
    mix(track, 0.0, whoosh(0.45), 0.5)
    for k, land in enumerate(LANDS):
        mix(track, land / FPS, boing([196, 247, 294][k]), 1.0)
        mix(track, (land + 1) / FPS, marimba(note(["C", "E", "G"][k], 5)), 0.6)
    mix(track, 21 / FPS, slide_whistle(500, 1700, 6 / FPS, seed=6), 0.8)
    mix(track, TV_AT / FPS, kick(), 1.0)
    mix(track, TV_AT / FPS, kazoo([note("C", 4), note("E", 4), note("G", 4), note("C", 5)], 0.25), 1.0)
    mix(track, (TV_AT + 3) / FPS, kazoo([note("D", 4), note("F", 4), note("A", 4), note("D", 5)], 0.18), 0.9)
    mix(track, (TV_AT + 5) / FPS, kazoo([note("E", 4), note("G", 4), note("C", 5), note("E", 5)], 0.9), 1.0)
    for k in range(6):
        mix(track, (TV_AT + k * 2) / FPS, hat(seed=k), 0.5)
    mix(track, LEAVE / FPS, slide_whistle(1300, 220, 0.5, seed=8), 0.9)
    mix(track, (LEAVE + 5) / FPS, bloop(260, 0.15, up=False), 0.9)
    write_wav(os.path.join(OUT, "bounce.wav"), track)
    return {"name": "bounce", "file": "bumper-bounce.mp4", "seconds": round(seconds, 3)}


os.makedirs(OUT, exist_ok=True)
made = []
for name, build in (("splat", bumper_splat), ("up-next", bumper_up_next), ("bounce", bumper_bounce)):
    if ONLY is None or name in ONLY:
        made.append(build())
if ONLY is None:
    with open(os.path.join(OUT, "bumpers.json"), "w") as f:
        json.dump(made, f, indent=1)
else:
    with open(os.path.join(OUT, "bumpers-partial.json"), "w") as f:
        json.dump(made, f, indent=1)
