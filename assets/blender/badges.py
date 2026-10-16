"""Renders the UI's small plastic pieces in the clock radio's materials and
light (radio.py), all facing the camera:

- splat.png: the top bar's orange splat, a soft molded badge on the outline
  splat.svg has (a little fuller), with its two droplets, a little in front of the bar so it
  casts a shadow on it. The lettering stays live text on the page.
- icon-likes, icon-youtube, icon-tiktok, icon-instagram, icon-x,
  icon-downloading (.png): the section icons. A pink heart, a red play
  tile, a black tile with TikTok's offset note, Instagram's gradient tile
  with its camera, a black tile with X's letter, and a green download button.

    blender -b -P assets/blender/badges.py -- /tmp/r [samples]
    for f in /tmp/r/*.png; do cwebp -q 90 -alpha_q 100 -m 6 $f -o assets/$(basename ${f%.png}).webp; done

The splat fills the `.splat` box (264 x 62 in style.css) as the SVG's
220 x 90 viewBox did, stretched to it; its image is that box plus a 12px
margin all round, at 4x. The icons are 160px squares.
"""

import glob
import math
import random
import sys

import bmesh
import bpy
import numpy as np
from mathutils import Vector
from mathutils.geometry import delaunay_2d_cdt

argv = sys.argv[sys.argv.index("--") + 1 :]
OUT_DIR = argv[0]
SAMPLES = int(argv[1]) if len(argv) > 1 else 512

bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene


def lin(hex_color):
    """sRGB hex to linear RGB."""
    out = []
    for i in (0, 2, 4):
        c = int(hex_color.lstrip("#")[i : i + 2], 16) / 255
        out.append(c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4)
    return out


def plastic(name, color, rough=0.3, coat=0.45, sss=0.1, grain=300.0):
    """radio.py's molded plastic: a clear coat, a little light scattering
    into the plastic, fine grain, and smudgy, uneven gloss."""
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    p.inputs["Base Color"].default_value = (*lin(color), 1)
    p.inputs["Coat Weight"].default_value = coat
    p.inputs["Coat Roughness"].default_value = 0.08
    if sss:
        p.inputs["Subsurface Weight"].default_value = sss
        p.inputs["Subsurface Radius"].default_value = (0.6, 0.6, 0.6)
        p.inputs["Subsurface Scale"].default_value = 0.3
    noise = nt.nodes.new("ShaderNodeTexNoise")
    noise.inputs["Scale"].default_value = grain
    noise.inputs["Detail"].default_value = 4
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = 0.06
    bump.inputs["Distance"].default_value = 0.02
    nt.links.new(noise.outputs["Fac"], bump.inputs["Height"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    smudge = nt.nodes.new("ShaderNodeTexNoise")
    smudge.inputs["Scale"].default_value = 0.3
    smudge.inputs["Detail"].default_value = 6
    smudge.inputs["Roughness"].default_value = 0.65
    mr = nt.nodes.new("ShaderNodeMapRange")
    mr.inputs["From Min"].default_value = 0.45
    mr.inputs["From Max"].default_value = 0.75
    mr.inputs["To Min"].default_value = rough * 0.85
    mr.inputs["To Max"].default_value = min(rough * 1.7, 0.9)
    nt.links.new(smudge.outputs["Fac"], mr.inputs["Value"])
    nt.links.new(mr.outputs["Result"], p.inputs["Roughness"])
    return m


M = {
    "orange": plastic("orange", "#ff7a00"),
    "pink": plastic("pink", "#ff3d7f", rough=0.22, coat=0.6, sss=0.2),
    "red": plastic("red", "#e62117", rough=0.25, coat=0.6),
    "black": plastic("black", "#141414", rough=0.3, coat=0.7, sss=0),
    "white": plastic("white", "#f6f6f2", rough=0.3, coat=0.4, sss=0.15),
    "cyan": plastic("cyan", "#25f4ee", rough=0.3, coat=0.4),
    "rose": plastic("rose", "#fe2c55", rough=0.3, coat=0.4),
    "green": plastic("green", "#3aa52c", rough=0.22, coat=0.6, sss=0.15),
}


def gradient_plastic(name, stops, origin, reach):
    """plastic() whose color runs through `stops` (position, hex) with the
    distance from `origin` (x, z), over `reach`: Instagram's glow, which
    starts yellow at the tile's lower left corner."""
    m = plastic(name, stops[0][1], rough=0.25, coat=0.6)
    nt = m.node_tree
    tc = nt.nodes.new("ShaderNodeTexCoord")
    sep = nt.nodes.new("ShaderNodeSeparateXYZ")
    flat = nt.nodes.new("ShaderNodeCombineXYZ")
    dist = nt.nodes.new("ShaderNodeVectorMath")
    dist.operation = "DISTANCE"
    dist.inputs[1].default_value = (origin[0], origin[1], 0)
    scale = nt.nodes.new("ShaderNodeMath")
    scale.operation = "DIVIDE"
    scale.inputs[1].default_value = reach
    ramp = nt.nodes.new("ShaderNodeValToRGB")
    elements = ramp.color_ramp.elements
    elements[0].position, elements[0].color = stops[0][0], (*lin(stops[0][1]), 1)
    elements[1].position, elements[1].color = stops[-1][0], (*lin(stops[-1][1]), 1)
    for pos, color in stops[1:-1]:
        elements.new(pos).color = (*lin(color), 1)
    nt.links.new(tc.outputs["Object"], sep.inputs["Vector"])
    nt.links.new(sep.outputs["X"], flat.inputs["X"])
    nt.links.new(sep.outputs["Z"], flat.inputs["Y"])
    nt.links.new(flat.outputs["Vector"], dist.inputs[0])
    nt.links.new(dist.outputs["Value"], scale.inputs[0])
    nt.links.new(scale.outputs["Value"], ramp.inputs["Fac"])
    nt.links.new(ramp.outputs["Color"], nt.nodes["Principled BSDF"].inputs["Base Color"])
    return m


M["instagram"] = gradient_plastic(
    "instagram",
    [(0.08, "#feda75"), (0.3, "#fa7e1e"), (0.55, "#d62976"), (0.8, "#962fbf"), (1.0, "#4f5bd5")],
    (-1.35, -1.55),
    4.6,
)


def link(ob, group):
    for c in ob.users_collection:
        c.objects.unlink(ob)
    group.objects.link(ob)
    return ob


def smooth(ob):
    for poly in ob.data.polygons:
        poly.use_smooth = True
    return ob


def pillow(name, outline, height, reach, step, mat, group, base=0.0):
    """A soft molded piece on an outline (x, z): a dense triangulated face,
    each point lifted toward the camera (-y) by its distance in from the
    edge, on a quarter circle that reaches full `height` `reach` in. Its
    edge rolls away like a pillow's; the middle is a broad dome or, with
    a short reach, flat. It sits with its back at y = base."""
    poly = np.array(outline)
    xs, zs = poly[:, 0], poly[:, 1]
    inner = []
    a, b = poly, np.roll(poly, -1, axis=0)
    for i, x in enumerate(np.arange(xs.min(), xs.max(), step)):
        for z in np.arange(zs.min(), zs.max(), step * 0.87):
            z = z + (step * 0.43 if i % 2 else 0)
            # Point in polygon, by counting edge crossings to the right.
            crosses = ((a[:, 1] > z) != (b[:, 1] > z)) & (
                x < (b[:, 0] - a[:, 0]) * (z - a[:, 1]) / (b[:, 1] - a[:, 1] + 1e-12) + a[:, 0]
            )
            if crosses.sum() % 2:
                inner.append((x, z))
    pts = [Vector(p) for p in outline] + [Vector(p) for p in inner]
    edges = [(i, (i + 1) % len(outline)) for i in range(len(outline))]
    verts, _, faces, *_ = delaunay_2d_cdt(pts, edges, [], 1, 1e-6)
    v = np.array([(p.x, p.y) for p in verts])
    ab = b - a
    t = np.clip(((v[:, None, :] - a) * ab).sum(-1) / (ab * ab).sum(-1), 0, 1)
    dist = np.sqrt(((v[:, None, :] - (a + t[..., None] * ab)) ** 2).sum(-1)).min(axis=1)
    lift = height * np.sqrt(1 - (1 - np.minimum(dist / reach, 1)) ** 2)
    bm = bmesh.new()
    front = [bm.verts.new((x, base - h, z)) for (x, z), h in zip(v, lift)]
    for f in faces:
        try:
            bm.faces.new([front[i] for i in f])
        except ValueError:
            continue
    back = [bm.verts.new((x, base + 0.01, z)) for x, z in outline]
    bm.faces.new(list(reversed(back)))
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    me = bpy.data.meshes.new(name)
    bm.to_mesh(me)
    bm.free()
    me.materials.append(mat)
    ob = smooth(bpy.data.objects.new(name, me))
    scene.collection.objects.link(ob)
    return link(ob, group)


def rrect(cx, cz, w, h, r, seg=16):
    pts = []
    for x, z, a0 in ((cx + w / 2 - r, cz + h / 2 - r, 0), (cx - w / 2 + r, cz + h / 2 - r, 90),
                     (cx - w / 2 + r, cz - h / 2 + r, 180), (cx + w / 2 - r, cz - h / 2 + r, 270)):
        for i in range(seg + 1):
            a = math.radians(a0 + 90 * i / seg)
            pts.append((x + r * math.cos(a), z + r * math.sin(a)))
    return pts


def stroke(name, points, radius, mat, group):
    """A round plastic rod along a polyline (x, y, z), with rounded ends."""
    cu = bpy.data.curves.new(name, "CURVE")
    cu.dimensions = "3D"
    cu.bevel_depth = radius
    cu.bevel_resolution = 8
    cu.use_fill_caps = True
    sp = cu.splines.new("POLY")
    sp.points.add(len(points) - 1)
    for p, co in zip(sp.points, points):
        p.co = (*co, 1)
    ob = bpy.data.objects.new(name, cu)
    ob.data.materials.append(mat)
    scene.collection.objects.link(ob)
    link(ob, group)
    for end in (points[0], points[-1]):
        ball(f"{name}-cap", end, radius, mat, group)
    return ob


def ball(name, loc, r, mat, group, squash=1.0):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=48, ring_count=24, radius=r, location=loc)
    ob = smooth(bpy.context.active_object)
    ob.name = name
    ob.scale = (1, squash, 1)
    ob.data.materials.append(mat)
    return link(ob, group)


def group(name):
    c = bpy.data.collections.new(name)
    scene.collection.children.link(c)
    return c


# ---- The splat ---------------------------------------------------------------------

BOX_W, BOX_H = 264, 62  # the .splat box, CSS px
# The image runs this far past the box on every side, for the shadow and
# the droplets; style.css draws it that much bigger than the box.
MARGIN = 12
UNIT = 0.1  # world units per CSS px


def splat_outline():
    """splat.svg's outline, in its viewBox (220 x 90, y down): an ellipse
    whose edge bulges in and out, smoothed through Catmull-Rom. Same seed
    and steps as the script that drew the SVG."""
    random.seed(7)
    w, h, n = 220, 90, 14
    cx, cy = w / 2, h / 2
    drips = {2: 1.18, 5: 1.12, 9: 1.2, 12: 1.1}
    ctrl = []
    for i in range(n):
        a = 2 * math.pi * i / n
        r = 1 + random.uniform(-0.04, 0.04) + (0.1 if i % 2 else -0.06)
        r *= drips.get(i, 1)
        ctrl.append((cx + math.cos(a) * (w / 2 - 14) * r * 0.93, cy + math.sin(a) * (h / 2 - 8) * r * 0.86))
    pts = []
    for i in range(n):
        p0, p1, p2, p3 = ctrl[i - 1], ctrl[i], ctrl[(i + 1) % n], ctrl[(i + 2) % n]
        for s in range(12):
            t = s / 12
            pts.append(
                tuple(
                    0.5 * (2 * p1[k] + (-p0[k] + p2[k]) * t + (2 * p0[k] - 5 * p1[k] + 4 * p2[k] - p3[k]) * t * t
                           + (-p0[k] + 3 * p1[k] - 3 * p2[k] + p3[k]) * t ** 3)
                    for k in (0, 1)
                )
            )
    return pts


def to_box(vx, vy):
    """A viewBox point to world (x, z), stretched to the box as the page did,
    and then a little more: as a solid piece it needs to be fuller than the
    flat SVG (whose offset shadow gave it weight) to hold the lettering."""
    return ((vx - 110) / 220 * BOX_W * UNIT * 1.06, (45 - vy) / 90 * BOX_H * UNIT * 1.25)


SPLAT = group("splat")
pillow("splat", [to_box(x, y) for x, y in splat_outline()], 1.2, 2.5, 0.12, M["orange"], SPLAT)
for vx, vy, r in ((18, 76, 5), (205, 14, 4)):
    x, z = to_box(vx, vy)
    ball("drop", (x, -0.4, z), r * BOX_H / 90 * UNIT, M["orange"], SPLAT, squash=0.55)
# The bar behind it: catches the splat's shadow, nothing else.
bpy.ops.mesh.primitive_plane_add(size=200, location=(0, 0.35, 0), rotation=(math.radians(90), 0, 0))
wall = link(bpy.context.active_object, SPLAT)
wall.is_shadow_catcher = True

# ---- The icons ------------------------------------------------------------------------
# Each is built round the origin, about 4 units across, facing -y.


def rounded_polygon(corners, r, seg=10):
    """A convex polygon, corners listed counterclockwise, each rounded to
    radius r."""
    pts = []
    n = len(corners)
    for i in range(n):
        p0, p1, p2 = (Vector(corners[(i + k) % n]) for k in (-1, 0, 1))
        d_in, d_out = (p1 - p0).normalized(), (p2 - p1).normalized()
        # Outward normals of the edges in and out (the outline is CCW).
        n_in, n_out = Vector((d_in.y, -d_in.x)), Vector((d_out.y, -d_out.x))
        # The arc's center sits r in from both edges.
        half = math.acos(max(-1, min(1, -d_in.dot(d_out)))) / 2
        center = p1 - (n_in + n_out).normalized() * (r / math.sin(half))
        a0, a1 = math.atan2(n_in.y, n_in.x), math.atan2(n_out.y, n_out.x)
        if a1 < a0:
            a1 += 2 * math.pi
        for k in range(seg + 1):
            a = a0 + (a1 - a0) * k / seg
            pts.append((center.x + r * math.cos(a), center.y + r * math.sin(a)))
    return pts


def heart_outline(size, n=160):
    pts = []
    for k in range(n):
        t = 2 * math.pi * k / n
        x = 16 * math.sin(t) ** 3
        z = 13 * math.cos(t) - 5 * math.cos(2 * t) - 2 * math.cos(3 * t) - math.cos(4 * t)
        pts.append((x * size / 34, (z + 2.5) * size / 34))
    return pts


LIKES = group("likes")
pillow("heart", heart_outline(3.9), 0.9, 1.4, 0.06, M["pink"], LIKES)

YOUTUBE = group("youtube")
pillow("tile", rrect(0, 0, 4.0, 2.9, 0.85), 0.55, 0.5, 0.06, M["red"], YOUTUBE)
# The play triangle, its corners rounded, standing on the tile's flat top.
pillow("play", rounded_polygon([(0.95, 0), (-0.55, 0.85), (-0.55, -0.85)], 0.2), 0.22, 0.2, 0.04, M["white"], YOUTUBE, base=-0.53)

TIKTOK = group("tiktok")
pillow("tile", rrect(0, 0, 3.7, 3.7, 0.95), 0.55, 0.5, 0.06, M["black"], TIKTOK)


def note(dx, dz, y, mat, name):
    """TikTok's note as rods: a stem, a curling flag, and a ring head."""
    stroke(f"{name}-stem", [(0.25 + dx, y, 1.15 + dz), (0.25 + dx, y, -0.45 + dz)], 0.2, mat, TIKTOK)
    # A quadratic curve hooking right and down from the top of the stem.
    flag = [
        (
            (1 - t) ** 2 * 0.25 + 2 * (1 - t) * t * 0.4 + t * t * 1.1 + dx,
            y,
            (1 - t) ** 2 * 1.15 + 2 * (1 - t) * t * 0.55 + t * t * 0.62 + dz,
        )
        for t in (k / 10 for k in range(11))
    ]
    stroke(f"{name}-flag", flag, 0.2, mat, TIKTOK)
    ring = [(-0.32 + dx + 0.52 * math.cos(math.radians(a)), y, -0.5 + dz + 0.52 * math.sin(math.radians(a))) for a in range(0, 361, 12)]
    stroke(f"{name}-head", ring, 0.2, mat, TIKTOK)


note(-0.09, 0.09, -0.62, M["cyan"], "cyan")
note(0.09, -0.09, -0.66, M["rose"], "rose")
note(0, 0, -0.78, M["white"], "white")

INSTAGRAM = group("instagram")
pillow("tile", rrect(0, 0, 3.7, 3.7, 1.05), 0.55, 0.5, 0.06, M["instagram"], INSTAGRAM)
# The camera, in white rods: a rounded square, the lens ring, the flash.
body = rrect(0, 0, 2.3, 2.3, 0.62, seg=10)
stroke("camera", [(x, -0.74, z) for x, z in body + body[:1]], 0.17, M["white"], INSTAGRAM)
lens = [(0.55 * math.cos(2 * math.pi * k / 48), -0.74, 0.55 * math.sin(2 * math.pi * k / 48)) for k in range(49)]
stroke("lens", lens, 0.17, M["white"], INSTAGRAM)
ball("flash", (0.6, -0.74, 0.6), 0.16, M["white"], INSTAGRAM)

X = group("x")
pillow("tile", rrect(0, 0, 3.7, 3.7, 0.95), 0.55, 0.5, 0.06, M["black"], X)
# X's letter in white rods: the falling stroke is drawn as an outline, as
# in the logo, and the rising one stops either side of it.
stroke("fall", [(x, -0.74, z) for x, z in ((-1.05, 1.1), (-0.6, 1.1), (1.05, -1.1), (0.6, -1.1), (-1.05, 1.1))], 0.12, M["white"], X)
stroke("rise-low", [(-0.95, -0.74, -1.1), (-0.24, -0.74, -0.28)], 0.12, M["white"], X)
stroke("rise-high", [(0.24, -0.74, 0.28), (0.95, -0.74, 1.1)], 0.12, M["white"], X)

DOWNLOAD = group("downloading")
circle = [(1.95 * math.cos(2 * math.pi * k / 160), 1.95 * math.sin(2 * math.pi * k / 160)) for k in range(160)]
pillow("button", circle, 0.75, 1.3, 0.06, M["green"], DOWNLOAD)
for name, pts in (
    ("shaft", [(0, -0.82, 1.0), (0, -0.82, -0.45)]),
    ("head", [(-0.72, -0.82, 0.1), (0, -0.82, -0.62), (0.72, -0.82, 0.1)]),
    ("base", [(-0.75, -0.72, -1.05), (0.75, -0.72, -1.05)]),
):
    stroke(name, pts, 0.21, M["white"], DOWNLOAD)

# ---- Light: radio.py's studio ------------------------------------------------------------

world = bpy.data.worlds.new("world")
scene.world = world
try:
    world.use_nodes = True
except AttributeError:
    pass
nt = world.node_tree
env = nt.nodes.new("ShaderNodeTexEnvironment")
env.image = bpy.data.images.load(glob.glob("/usr/share/blender/*/datafiles/studiolights/world/studio.exr")[0])
bg = nt.nodes["Background"]
bg.inputs["Strength"].default_value = 0.45
nt.links.new(env.outputs["Color"], bg.inputs["Color"])
# The studio lights reflections only: lighting the splat's wall with it
# would have the catcher record its occlusion as a haze all round.
path = nt.nodes.new("ShaderNodeLightPath")
dark = nt.nodes.new("ShaderNodeBackground")
dark.inputs["Strength"].default_value = 0.0
mix = nt.nodes.new("ShaderNodeMixShader")
nt.links.new(path.outputs["Is Glossy Ray"], mix.inputs["Fac"])
nt.links.new(dark.outputs["Background"], mix.inputs[1])
nt.links.new(bg.outputs["Background"], mix.inputs[2])
nt.links.new(mix.outputs["Shader"], nt.nodes["World Output"].inputs["Surface"])


def area(name, loc, power, size, color=(1, 1, 1)):
    data = bpy.data.lights.new(name, "AREA")
    data.energy = power
    data.size = size
    data.color = color
    ob = bpy.data.objects.new(name, data)
    scene.collection.objects.link(ob)
    ob.location = loc
    ob.rotation_euler = (-Vector(loc)).to_track_quat("-Z", "Y").to_euler()


# Key from the upper left, so the splat's shadow falls down and right as the
# SVG's offset copy did; a soft fill; a violet kicker like the radio's.
area("key", (-25, -30, 35), 42000, 30)
area("fill", (30, -35, 0), 9000, 30, color=(0.9, 0.95, 1.0))
area("violet", (20, 10, 25), 6000, 15, color=(0.55, 0.3, 1.0))

cam_data = bpy.data.cameras.new("cam")
cam_data.type = "ORTHO"
cam = bpy.data.objects.new("cam", cam_data)
scene.collection.objects.link(cam)
cam.location = (0, -50, 0)
cam.rotation_euler = (math.radians(90), 0, 0)
scene.camera = cam

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
scene.render.film_transparent = True
scene.render.image_settings.file_format = "PNG"
scene.render.image_settings.color_mode = "RGBA"
# As radio.py: Standard keeps the toy colors true.
scene.view_settings.view_transform = "Standard"


def render(only, name, width, height, ortho, exposure=0.0):
    for c in scene.collection.children:
        c.hide_render = c is not only
    cam_data.ortho_scale = ortho
    scene.render.resolution_x = width
    scene.render.resolution_y = height
    scene.view_settings.exposure = exposure
    scene.render.filepath = f"{OUT_DIR}/{name}.png"
    bpy.ops.render.render(write_still=True)
    return scene.render.filepath


out = render(SPLAT, "splat", (BOX_W + 2 * MARGIN) * 4, (BOX_H + 2 * MARGIN) * 4, (BOX_W + 2 * MARGIN) * UNIT)
# The catcher leaves faint specks far from the splat; keep the shadow, cap
# how dark it gets, and drop the faintest.
img = bpy.data.images.load(out)
px = np.empty(len(img.pixels), dtype=np.float32)
img.pixels.foreach_get(px)
px = px.reshape(-1, 4)
shadow = px[:, :3].max(axis=1) < 0.002
px[:, 3] = np.where(shadow, np.where(px[:, 3] < 0.03, 0, np.minimum(px[:, 3], 0.55)), px[:, 3])
img.pixels.foreach_set(px.ravel())
img.filepath_raw = out
img.file_format = "PNG"
img.save()

for g in (LIKES, YOUTUBE, TIKTOK, INSTAGRAM, X, DOWNLOAD):
    render(g, f"icon-{g.name}", 160, 160, 4.2, exposure=-0.15)
