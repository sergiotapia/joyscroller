"""Renders /tv's television set in the clock radio's plastic and light
(radio.py): an early-90s color set with a 4:3 picture tube, molded like the
radio, with its tank-tread shell, a bright purple trim round a dark panel,
an orange splat badge, a lime plinth, and rabbit ears topped with the
radio's lime balls. The camera looks straight at the front, from up and to
the right with a shifted lens, so the front's outlines (the screen's
opening above all) land on the image as true rectangles while the top and
right side still show.

- tv.png: the set, with the picture tube's glass cut out (transparent),
  where the page draws the picture (assets/crt.js) under it.
- tv-glass.png: the glass alone: what it reflects of the studio, on
  transparency. The page lays it over the picture with `screen` blending,
  as reflections on a tube's face add to its light.
- tv-power.png: the red mushroom POWER button, up then pressed.
- tv-down.png, tv-up.png: the channel buttons, up then pressed.
- tv-knob.png: the VOLUME knob turned from KNOB_FROM to KNOB_TO degrees in
  KNOB_FRAMES steps, one above the other.
- tv-led.png: the power light, off, red (standby) then green (on).
- tv.json: where all of that goes in tv.png, in its pixels, and where the
  channel LCD's glass is, which the page lights with the channel's number.

    blender -b -P assets/blender/tv.py -- /tmp/r /path/to/fonts [samples]

`just tv` runs it and converts the images. The fonts dir needs
LuckiestGuy.ttf and Fredoka.ttf (see the justfile). Each sprite is the
still's own pixels with the part moved, cut from the same camera and
light, so it lines up with the still; its edges fade into the still over
FEATHER pixels, where the two are the same.
"""

import glob
import json
import math
import os
import sys

import bmesh
import bpy
import numpy as np
from bpy_extras.object_utils import world_to_camera_view
from mathutils import Matrix, Vector

argv = sys.argv[sys.argv.index("--") + 1 :]
OUT_DIR = argv[0]
FONTS = argv[1]
SAMPLES = int(argv[2]) if len(argv) > 2 else 256
# The width of tv.png: twice the widest the page shows the set, for sharp
# edges on a high-density screen.
RES_X = int(argv[3]) if len(argv) > 3 else 2400

KNOB_FROM, KNOB_TO, KNOB_FRAMES = -135.0, 135.0, 28
FEATHER = 6
os.makedirs(os.path.join(OUT_DIR, "frames"), exist_ok=True)

bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene


def lin(hex_color):
    """sRGB hex to linear RGB."""
    out = []
    for i in (0, 2, 4):
        c = int(hex_color.lstrip("#")[i : i + 2], 16) / 255
        out.append(c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4)
    return out


def material(name, color, rough=0.35, coat=0.0, sss=0.0, metallic=0.0, ior=1.45):
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    p = m.node_tree.nodes["Principled BSDF"]
    p.inputs["Base Color"].default_value = (*lin(color), 1)
    p.inputs["Roughness"].default_value = rough
    p.inputs["Metallic"].default_value = metallic
    p.inputs["IOR"].default_value = ior
    p.inputs["Coat Weight"].default_value = coat
    p.inputs["Coat Roughness"].default_value = 0.08
    if sss:
        p.inputs["Subsurface Weight"].default_value = sss
        p.inputs["Subsurface Radius"].default_value = (0.6, 0.6, 0.6)
        p.inputs["Subsurface Scale"].default_value = 0.3
    return m


def plastic_grain(m, scale=300.0, strength=0.06, smudge_scale=0.3):
    """radio.py's molded plastic: fine grain and smudgy, uneven gloss. The
    set is three times the radio's size, so the grain is three times
    coarser in world units, and the same on screen."""
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    noise = nt.nodes.new("ShaderNodeTexNoise")
    noise.inputs["Scale"].default_value = scale
    noise.inputs["Detail"].default_value = 4
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = strength
    bump.inputs["Distance"].default_value = 0.06
    nt.links.new(noise.outputs["Fac"], bump.inputs["Height"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    smudge = nt.nodes.new("ShaderNodeTexNoise")
    smudge.inputs["Scale"].default_value = smudge_scale
    smudge.inputs["Detail"].default_value = 6
    smudge.inputs["Roughness"].default_value = 0.65
    rough = p.inputs["Roughness"].default_value
    mr = nt.nodes.new("ShaderNodeMapRange")
    mr.inputs["From Min"].default_value = 0.45
    mr.inputs["From Max"].default_value = 0.75
    mr.inputs["To Min"].default_value = rough * 0.85
    mr.inputs["To Max"].default_value = min(rough * 1.7, 0.9)
    nt.links.new(smudge.outputs["Fac"], mr.inputs["Value"])
    nt.links.new(mr.outputs["Result"], p.inputs["Roughness"])
    return m


# radio.py's colors, so the two read as one product line.
M = {
    "body": plastic_grain(material("body", "#1b1452", rough=0.42, coat=0.08)),
    "panel": plastic_grain(material("panel", "#221a63", rough=0.36, coat=0.2)),
    "purple": plastic_grain(material("purple", "#7426f2", rough=0.28, coat=0.4, sss=0.08)),
    "violet": plastic_grain(material("violet", "#8f5cf5", rough=0.25, coat=0.6, sss=0.15)),
    "lime": plastic_grain(material("lime", "#8be01a", rough=0.3, coat=0.35, sss=0.15)),
    "ball": material("ball", "#82dc1c", rough=0.16, coat=0.7, sss=0.3),
    "red": material("red", "#e3231a", rough=0.12, coat=0.8, sss=0.35),
    "orange": plastic_grain(material("orange", "#ff5e00", rough=0.3, coat=0.45, sss=0.1)),
    "white": material("white", "#f7f7f2", rough=0.4),
    "ink": material("ink", "#120c1e", rough=0.5),
    "print": material("print", "#b6f23a", rough=0.5),
    "gasket": plastic_grain(material("gasket", "#0d0a1c", rough=0.55, coat=0.1)),
    "slot": material("slot", "#05040a", rough=0.8),
    "chrome": material("chrome", "#d9dbe6", rough=0.22, metallic=1.0),
    "dark": material("dark", "#0d0b16", rough=0.6),
}


def lcd_material():
    """radio.py's LCD switched to black: near-black green under a glossy
    cover. The page draws the lit digits over it."""
    m = material("lcd", "#040705", rough=0.5, coat=0.45)
    m.node_tree.nodes["Principled BSDF"].inputs["Coat Roughness"].default_value = 0.03
    return m


def glass_material():
    """A picture tube's face plate: thick, dark-tinted glass, polished. Seen
    from the front it's nearly black, and what shows is what it reflects,
    more at a glance than straight on."""
    m = material("glass", "#000000", rough=0.05, ior=1.52)
    p = m.node_tree.nodes["Principled BSDF"]
    p.inputs["Specular IOR Level"].default_value = 0.7
    return m


def led_material(name, color, glow):
    m = material(name, color, rough=0.08, coat=0.9, sss=0.5)
    p = m.node_tree.nodes["Principled BSDF"]
    p.inputs["Emission Color"].default_value = (*lin(color), 1)
    p.inputs["Emission Strength"].default_value = glow
    return m


M["lcd"] = lcd_material()
M["glass"] = glass_material()
M["led-off"] = led_material("led-off", "#3a0d0a", 0.0)
M["led-red"] = led_material("led-red", "#ff2a14", 3.0)
M["led-green"] = led_material("led-green", "#5cff1a", 2.2)


# ---- Geometry helpers (as radio.py's) --------------------------------------


def link(ob):
    scene.collection.objects.link(ob)
    return ob


def from_bmesh(name, bm, mat, smooth=True):
    me = bpy.data.meshes.new(name)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    bm.to_mesh(me)
    bm.free()
    me.materials.append(mat)
    for poly in me.polygons:
        poly.use_smooth = smooth
    return link(bpy.data.objects.new(name, me))


def bevel(ob, width, segments=4, angle=30):
    mod = ob.modifiers.new("bevel", "BEVEL")
    mod.width = width
    mod.segments = segments
    mod.limit_method = "ANGLE"
    mod.angle_limit = math.radians(angle)
    mod.harden_normals = True
    return ob


def rrect(cx, cz, w, h, r, seg=14):
    pts = []
    corners = [
        (cx + w / 2 - r, cz + h / 2 - r, 0),
        (cx - w / 2 + r, cz + h / 2 - r, 90),
        (cx - w / 2 + r, cz - h / 2 + r, 180),
        (cx + w / 2 - r, cz - h / 2 + r, 270),
    ]
    for x, z, a0 in corners:
        for i in range(seg + 1):
            a = math.radians(a0 + 90 * i / seg)
            pts.append((x + r * math.cos(a), z + r * math.sin(a)))
    return pts


def prism(name, pts, y0, depth, mat, bev=0.12, segs=4):
    """Extrude a front-view outline (x, z) back along +y from y0."""
    bm = bmesh.new()
    front = [bm.verts.new((x, y0, z)) for x, z in pts]
    back = [bm.verts.new((x, y0 + depth, z)) for x, z in pts]
    n = len(pts)
    bm.faces.new(front)
    bm.faces.new(list(reversed(back)))
    for i in range(n):
        j = (i + 1) % n
        bm.faces.new((front[i], front[j], back[j], back[i]))
    ob = from_bmesh(name, bm, mat)
    if bev:
        bevel(ob, bev, segs)
    return ob


def frame(name, outer, inner, y0, depth, mat, bev=0.06):
    """A ring between two outlines with the same number of points."""
    bm = bmesh.new()
    of = [bm.verts.new((x, y0, z)) for x, z in outer]
    inf = [bm.verts.new((x, y0, z)) for x, z in inner]
    ob_ = [bm.verts.new((x, y0 + depth, z)) for x, z in outer]
    ib = [bm.verts.new((x, y0 + depth, z)) for x, z in inner]
    n = len(outer)
    for i in range(n):
        j = (i + 1) % n
        bm.faces.new((of[i], of[j], inf[j], inf[i]))
        bm.faces.new((ob_[i], ib[i], ib[j], ob_[j]))
        bm.faces.new((of[i], ob_[i], ob_[j], of[j]))
        bm.faces.new((inf[i], inf[j], ib[j], ib[i]))
    ob = from_bmesh(name, bm, mat)
    return bevel(ob, bev, 3)


def lathe(name, profile, center, mat, axis="z", segs=96, bev=0.05):
    """Spin a (radius, height) profile, bottom to top, about an axis through
    `center`: up (z), or toward the camera (-y) for a part on the front."""
    bm = bmesh.new()
    rings = []
    for r, h in profile:
        ring = []
        for k in range(segs):
            t = 2 * math.pi * k / segs
            if axis == "z":
                co = (r * math.cos(t), r * math.sin(t), h)
            else:
                co = (r * math.cos(t), -h, r * math.sin(t))
            ring.append(bm.verts.new(Vector(center) + Vector(co)))
        rings.append(ring)
    for a, b in zip(rings, rings[1:]):
        for k in range(segs):
            m = (k + 1) % segs
            bm.faces.new((a[k], a[m], b[m], b[k]))
    bm.faces.new(list(reversed(rings[0])))
    bm.faces.new(rings[-1])
    ob = from_bmesh(name, bm, mat)
    return bevel(ob, bev, 3) if bev else ob


def sphere(name, loc, r, mat):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=96, ring_count=48, radius=r, location=loc)
    ob = bpy.context.active_object
    ob.name = name
    for poly in ob.data.polygons:
        poly.use_smooth = True
    ob.data.materials.append(mat)
    return ob


def text(name, body, font, size, loc, mat, rot=0.0, extrude=0.03, offset=0.0):
    cu = bpy.data.curves.new(name, "FONT")
    cu.body = body
    cu.font = bpy.data.fonts.load(font)
    cu.size = size
    cu.extrude = extrude
    cu.offset = offset
    cu.align_x = "CENTER"
    cu.align_y = "CENTER"
    ob = link(bpy.data.objects.new(name, cu))
    ob.location = loc
    ob.rotation_euler = (math.radians(90), math.radians(rot), 0)
    ob.data.materials.append(mat)
    return ob


def printed(name, body, font, size, loc, rot=0.0):
    """White lettering with a black outline, as radio.py prints JOY!."""
    text(f"{name}-edge", body, font, size, (loc[0], loc[1] + 0.04, loc[2]), M["ink"], rot=rot, offset=size * 0.05)
    return text(name, body, font, size, (loc[0], loc[1] - 0.04, loc[2]), M["white"], rot=rot)


def rrect_walk(cx, cz, w, h, r):
    """Walk round a rounded rectangle by arc length (radio.py's): returns
    (length, at), where at(s) gives the point and outward normal."""
    a, b = w / 2 - r, h / 2 - r
    pieces = [
        ("line", (cx + w / 2, cz - b), (cx + w / 2, cz + b), (1, 0)),
        ("arc", (cx + a, cz + b), 0),
        ("line", (cx + a, cz + h / 2), (cx - a, cz + h / 2), (0, 1)),
        ("arc", (cx - a, cz + b), 90),
        ("line", (cx - w / 2, cz + b), (cx - w / 2, cz - b), (-1, 0)),
        ("arc", (cx - a, cz - b), 180),
        ("line", (cx - a, cz - h / 2), (cx + a, cz - h / 2), (0, -1)),
        ("arc", (cx + a, cz - b), 270),
    ]
    sizes = [math.dist(p[1], p[2]) if p[0] == "line" else r * math.pi / 2 for p in pieces]
    total = sum(sizes)

    def at(s):
        s %= total
        for piece, size in zip(pieces, sizes):
            if s <= size or piece is pieces[-1]:
                t = s / size if size else 0
                if piece[0] == "line":
                    (x0, z0), (x1, z1), n = piece[1], piece[2], piece[3]
                    return x0 + (x1 - x0) * t, z0 + (z1 - z0) * t, n[0], n[1]
                (ox, oz), a0 = piece[1], piece[2]
                ang = math.radians(a0 + 90 * t)
                return ox + r * math.cos(ang), oz + r * math.sin(ang), math.cos(ang), math.sin(ang)
            s -= size
        raise AssertionError

    return total, at


def toothed(walk, depth, period, shape=(0.0, 0.08, 0.86, 0.94)):
    """radio.py's tank-tread edge: square-ish teeth standing out from an
    outline."""
    total, at = walk
    n = max(6, round(total / period))
    step = total / n
    pts = []
    for k in range(n):
        for frac, out in zip(shape, (0, 1, 1, 0)):
            x, z, nx, nz = at((k + frac) * step)
            pts.append((x + nx * depth * out, z + nz * depth * out))
    return pts


def burst(cx, cz, rx, rz, spikes, depth, rot):
    """radio.py's flame burst: an ellipse with leaning sawtooth spikes."""
    pts = []
    for i in range(spikes * 2):
        lean = 0.35 if i % 2 == 0 else 0.0
        a = 2 * math.pi * (i + lean) / (spikes * 2)
        k = 1 + depth if i % 2 == 0 else 1 - depth * 0.35
        pts.append((math.cos(a) * rx * k, math.sin(a) * rz * k))
    c, s_ = math.cos(math.radians(rot)), math.sin(math.radians(rot))
    return [(cx + x * c - z * s_, cz + x * s_ + z * c) for x, z in pts]


def cylinder_y(name, cx, cz, r, y0, depth, mat, bev=0.05, verts=96):
    """A disc facing the camera, from y0 back by `depth`."""
    bpy.ops.mesh.primitive_cylinder_add(vertices=verts, radius=r, depth=depth, location=(cx, y0 + depth / 2, cz), rotation=(math.radians(90), 0, 0))
    ob = bpy.context.active_object
    ob.name = name
    ob.data.materials.append(mat)
    if bev:
        bevel(ob, bev, 3)
    return ob


def arc_text(name, body, font, size, cx, cz, r, a0, a1, y, mat):
    """Letters set round an arc, from angle a0 to a1 (degrees)."""
    for k, ch in enumerate(body):
        t = math.radians(a0 + (a1 - a0) * (k + 0.5) / len(body))
        text(f"{name}{k}", ch, font, size, (cx + r * math.cos(t), y, cz + r * math.sin(t)), mat, rot=-(math.degrees(t) - 90))


def group(name, obs, pivot):
    """Parent `obs` to an empty at `pivot`, to move them together."""
    empty = link(bpy.data.objects.new(name, None))
    empty.location = pivot
    bpy.context.view_layer.update()
    for ob in obs:
        world = ob.matrix_world.copy()
        ob.parent = empty
        ob.matrix_world = world
    return empty


def made_since(before):
    return [ob for ob in scene.objects if ob.name not in before]


LUCKIEST = f"{FONTS}/LuckiestGuy.ttf"
FREDOKA = f"{FONTS}/Fredoka.ttf"

# ---- Body --------------------------------------------------------------------
# Units are centimeters: a 20-inch set (its picture 41 x 31 cm), as a living
# room had. The body sits on the plinth, its front face toward -y.

W, H, D = 64.0, 47.5, 40.0
R = 6.0
TEETH = 0.62
PLINTH = 1.6
YF = -D / 2

# The picture tube's opening: 4:3, set in a dark gasket, left of the
# control column.
GW = 1.6  # the gasket's width
SW, SH, SR = 44.0, 33.0, 3.2
SX, SZ = -W / 2 + 3.1 + GW + SW / 2, 3.1 + GW + 5.0 + SH / 2
# The control column's center, between the gasket and the panel's edge.
CX = (SX + SW / 2 + GW + W / 2 - 3.1) / 2

shell = M["body"]
tread = toothed(rrect_walk(0, H / 2, W, H, R), TEETH, 4.0)
prism("body", tread, YF, D, shell, bev=0.18, segs=3)
prism("core", rrect(0, H / 2, W - 0.05, H - 0.05, R - 0.02, seg=32), YF + 0.4, D - 0.8, shell, bev=0.8, segs=4)

# A bright purple trim inside the teeth, round a raised panel with the
# screen's hole in it.
frame("trim", rrect(0, H / 2, W - 0.3, H - 0.3, R - 0.15, seg=24), rrect(0, H / 2, W - 2.4, H - 2.4, R - 1.2, seg=24), YF - 0.6, 0.8, M["purple"], bev=0.2)
YP = YF - 0.35  # the panel's front
frame(
    "panel",
    rrect(0, H / 2, W - 2.4, H - 2.4, R - 1.2, seg=24),
    rrect(SX, SZ, SW + 2 * GW + 0.4, SH + 2 * GW + 0.4, SR + GW + 0.2, seg=24),
    YP,
    0.5,
    M["panel"],
    bev=0.2,
)
# The gasket: a dark rubbery ring round the tube's face, standing a little
# proud of the panel, its inner edge the opening the picture shows through.
frame(
    "gasket",
    rrect(SX, SZ, SW + 2 * GW, SH + 2 * GW, SR + GW, seg=24),
    rrect(SX, SZ, SW, SH, SR, seg=24),
    YP - 0.35,
    1.6,
    M["gasket"],
    bev=0.25,
)
# The tube's face behind it, bowed out a little, as tubes were. It's cut
# out of the still (a holdout); tv-glass.png is its reflections.
GLASS_Y = YF - 0.05  # the face's edge, inside the gasket, in front of the body
BULGE = 2.4  # a 20-inch tube's face bows out about this much
bpy.ops.mesh.primitive_grid_add(x_subdivisions=160, y_subdivisions=120, size=1, location=(SX, GLASS_Y, SZ), rotation=(math.radians(90), 0, 0))
glass = bpy.context.active_object
glass.name = "glass"
glass.scale = (SW + 1.2, SH + 1.2, 1)
bpy.context.view_layer.update()
gm = glass.data
inverse = glass.matrix_world.inverted()
for v in gm.vertices:
    w_ = glass.matrix_world @ v.co
    u, t = (w_.x - SX) / (SW / 2 + 0.6), (w_.z - SZ) / (SH / 2 + 0.6)
    # Bowed in both directions, flat at the edges, which sit behind the
    # gasket; the middle stands proud of it.
    w_.y -= BULGE * max(0.0, 1 - u * u) * max(0.0, 1 - t * t)
    v.co = inverse @ w_
# Rounded at the corners like the opening, so no corner of it peeks out
# past the gasket.
bm = bmesh.new()
bm.from_mesh(gm)
reach = (SW / 2 + 0.6 - (SR + 0.6), SH / 2 + 0.6 - (SR + 0.6))


def outside(v):
    w_ = glass.matrix_world @ v.co
    qx, qz = max(abs(w_.x - SX) - reach[0], 0.0), max(abs(w_.z - SZ) - reach[1], 0.0)
    return math.hypot(qx, qz) > SR + 0.6


bmesh.ops.delete(bm, geom=[v for v in bm.verts if outside(v)], context="VERTS")
bm.to_mesh(gm)
bm.free()
for poly in gm.polygons:
    poly.use_smooth = True
gm.materials.append(M["glass"])

# ---- Front: under the screen --------------------------------------------------

LIP_Z = 3.1 + 2.5  # the middle of the strip under the gasket
# The splat badge, as on the radio's speaker.
BX, BZ = SX - SW / 2 + 7.0, LIP_Z + 0.4
splat = burst(BX, BZ, 5.6, 2.5, 12, 0.24, 8)
prism("badge-shadow", splat, YP - 0.3, 0.2, M["ink"], bev=0.04, segs=2).location = (0.14, 0, -0.2)
prism("badge", splat, YP - 0.55, 0.3, M["orange"], bev=0.09, segs=3)
printed("badge-text", "JOY TV", LUCKIEST, 2.3, (BX + 0.1, YP - 0.62, BZ - 0.05), rot=-8)

# The speaker: slots across the strip, recessed in a lime surround.
SPX0, SPX1 = BX + 8.5, SX + SW / 2 - 1.0
SPW = SPX1 - SPX0
frame("speaker-ring", rrect((SPX0 + SPX1) / 2, LIP_Z, SPW + 1.4, 4.2, 1.2, seg=8), rrect((SPX0 + SPX1) / 2, LIP_Z, SPW, 2.8, 0.6, seg=8), YP - 0.45, 0.5, M["lime"], bev=0.12)
for k in range(4):
    z = LIP_Z - 0.95 + k * 0.63
    prism(f"slot{k}", rrect((SPX0 + SPX1) / 2, z, SPW - 0.8, 0.32, 0.15, seg=4), YP - 0.05, 0.2, M["slot"], bev=0.03, segs=2)

# ---- Front: the control column -------------------------------------------------

# The channel LCD at the top, in the radio's purple bezel.
LCD_W, LCD_H = 7.6, 3.8
LCD_Z = SZ + SH / 2 - 2.6
frame("lcd-bezel", rrect(CX, LCD_Z, LCD_W + 1.4, LCD_H + 1.4, 0.8, seg=8), rrect(CX, LCD_Z, LCD_W, LCD_H, 0.3, seg=8), YP - 0.5, 0.6, M["purple"], bev=0.1)
bpy.ops.mesh.primitive_plane_add(size=1, location=(CX, YP - 0.05, LCD_Z), rotation=(math.radians(90), 0, 0))
lcd = bpy.context.active_object
lcd.name = "lcd"
lcd.scale = (LCD_W, LCD_H, 1)
lcd.data.materials.append(M["lcd"])
text("lcd-label", "CHANNEL", FREDOKA, 0.75, (CX, YP - 0.06, LCD_Z - LCD_H / 2 - 1.35), M["print"])

# The channel buttons, side by side in a lime frame: round violet caps
# with arrows printed on them.
CH_Z = LCD_Z - 7.6
CH_R = 1.55
CH_DX = 2.15
frame("ch-frame", rrect(CX, CH_Z, 2 * CH_DX + 2 * CH_R + 1.6, 2 * CH_R + 1.6, CH_R + 0.8, seg=12), rrect(CX, CH_Z, 2 * CH_DX + 2 * CH_R + 0.4, 2 * CH_R + 0.4, CH_R + 0.2, seg=12), YP - 0.4, 0.45, M["lime"], bev=0.1)
bpy.ops.mesh.primitive_plane_add(size=1, location=(CX, YP - 0.02, CH_Z), rotation=(math.radians(90), 0, 0))
well = bpy.context.active_object
well.scale = (2 * CH_DX + 2 * CH_R + 0.5, 2 * CH_R + 0.5, 1)
well.data.materials.append(M["body"])
CAP_Y = YP - 1.05
BUTTONS = {}
for name, dx, up in (("down", -CH_DX, False), ("up", CH_DX, True)):
    before = {ob.name for ob in scene.objects}
    cylinder_y(f"ch-{name}", CX + dx, CH_Z, CH_R, CAP_Y, 1.0, M["violet"], bev=0.35)
    s = 0.62 if up else -0.62
    arrow = [(CX + dx - 0.75, CH_Z - s * 0.45), (CX + dx + 0.75, CH_Z - s * 0.45), (CX + dx, CH_Z + s * 0.6)]
    prism(f"ch-{name}-arrow", arrow, CAP_Y - 0.06, 0.08, M["white"], bev=0.03, segs=2)
    BUTTONS[name] = (group(f"ch-{name}-group", made_since(before), (CX + dx, CAP_Y, CH_Z)), (CX + dx, CH_Z, CH_R + 0.4))
text("ch-label", "CH", FREDOKA, 0.75, (CX, YP - 0.06, CH_Z - CH_R - 1.75), M["print"])

# The VOLUME knob: radio.py's knob, a round cap with a raised bar across
# it and a white pointer dot at the bar's end, pointing straight up at
# its middle setting.
VOL_Z = CH_Z - 8.4
VOL_R = 2.35
before = {ob.name for ob in scene.objects}
cylinder_y("knob", CX, VOL_Z, VOL_R, YP - 1.7, 1.7, M["purple"], bev=0.3)
bpy.ops.mesh.primitive_cube_add(size=1, location=(CX, YP - 1.7 - 0.3, VOL_Z))
bar = bpy.context.active_object
bar.name = "knob-bar"
bar.scale = (VOL_R * 0.42, 0.8, VOL_R * 1.75)
bar.data.materials.append(M["purple"])
bevel(bar, 0.2, 3)
cylinder_y("knob-dot", CX, VOL_Z + VOL_R * 0.68, VOL_R * 0.12, YP - 1.7 - 0.72, 0.1, M["white"], bev=0.02, verts=32)
knob = group("knob-group", made_since(before), (CX, YP, VOL_Z))
arc_text("volume-label", "VOLUME", LUCKIEST, 0.95, CX, VOL_Z, VOL_R + 1.3, 150, 30, YP - 0.06, M["print"])
for k in range(11):
    a = math.radians(90) - math.radians(KNOB_FROM + (KNOB_TO - KNOB_FROM) * k / 10)
    cylinder_y(f"vol-tick{k}", CX + (VOL_R + 0.55) * math.cos(a), VOL_Z + (VOL_R + 0.55) * math.sin(a), 0.11 if k % 5 else 0.17, YP - 0.06, 0.05, M["print"], bev=0.0, verts=16)

# POWER: the radio's red mushroom dome, mounted on the front, in a lime
# collar on a purple base, with the power light beside it.
PW_Z = 3.1 + 7.6
PW_R = 2.9
lathe("power-base", [(PW_R + 0.9, 0), (PW_R + 0.9, 0.7), (PW_R + 0.5, 0.95), (PW_R, 1.0)], (CX, YP, PW_Z), M["purple"], axis="y", bev=0.15)
lathe("power-collar", [(PW_R - 0.3, 0), (PW_R - 0.3, 0.45), (PW_R - 0.5, 0.55)], (CX, YP - 0.9, PW_Z), M["lime"], axis="y")
before = {ob.name for ob in scene.objects}
dome_y = YP - 1.35
dome = sphere("power", (CX, dome_y, PW_Z), PW_R, M["red"])
dome.scale = (1, 0.6, 1)
bm = bmesh.new()
bm.from_mesh(dome.data)
bmesh.ops.delete(bm, geom=[v for v in bm.verts if v.co.y > 1e-4], context="VERTS")
bm.to_mesh(dome.data)
bm.free()
lathe("power-lip", [(PW_R - 0.1, 0), (PW_R + 0.1, 0.12), (PW_R + 0.08, 0.35), (PW_R - 0.1, 0.4)], (CX, dome_y + 0.25, PW_Z), M["red"], axis="y", bev=0.04)
# The power symbol, printed on the cap's crown.
ring = []
for k in range(41):
    a = math.radians(-50 - 260 * k / 40)
    ring.append((CX + 1.2 * math.sin(a), PW_Z + 1.2 * math.cos(a)))
inner = [(CX + 0.82 * math.sin(math.radians(-50 - 260 * k / 40)), PW_Z + 0.82 * math.cos(math.radians(-50 - 260 * k / 40))) for k in range(41)]
prism("power-ring", ring + list(reversed(inner)), dome_y - PW_R * 0.6 - 0.02, 0.06, M["white"], bev=0.0)
prism("power-bar", rrect(CX, PW_Z + 0.62, 0.38, 1.55, 0.18, seg=4), dome_y - PW_R * 0.6 - 0.03, 0.06, M["white"], bev=0.0)
for ob in ("power-ring", "power-bar"):
    # Wrap the flat print onto the dome's curve: push each vertex back to
    # the cap's surface.
    o = bpy.data.objects[ob]
    for v in o.data.vertices:
        dx, dz = v.co.x - CX, v.co.z - PW_Z
        rr = min(1.0, (dx * dx + dz * dz) / (PW_R * PW_R))
        v.co.y = dome_y - PW_R * 0.6 * math.sqrt(max(0.0, 1 - rr)) - 0.025 + (v.co.y - (dome_y - PW_R * 0.6 - 0.02))
power = group("power-group", made_since(before), (CX, dome_y, PW_Z))
text("power-label", "POWER", LUCKIEST, 0.95, (CX - 0.9, YP - 0.06, 3.1 + 2.3), M["print"])
LED_X, LED_Z = CX + 2.9, 3.1 + 2.35
cylinder_y("led-ring", LED_X, LED_Z, 0.62, YP - 0.25, 0.3, M["purple"], bev=0.08, verts=48)
led = sphere("led", (LED_X, YP - 0.3, LED_Z), 0.4, M["led-off"])

# ---- Top: rabbit ears -------------------------------------------------------------

TOP = H + TEETH
AX = SX + 4.0
lathe("ears-base", [(4.0, 0), (4.0, 0.8), (3.6, 1.4), (2.6, 2.1), (1.6, 2.5), (1.0, 2.6)], (AX, 2.0, TOP - 0.2), M["purple"], bev=0.2)
lathe("ears-collar", [(1.15, 0), (1.15, 0.6), (0.95, 0.75)], (AX, 2.0, TOP + 2.2), M["lime"])
EARS = []
for side in (-1, 1):
    # Telescoping chrome rods, splayed out, leaning back a little.
    direction = Vector((side * math.sin(math.radians(54)), 0.18, math.cos(math.radians(54)))).normalized()
    start = Vector((AX, 2.0, TOP + 2.6))
    at = start
    # Short and splayed wide, so they rise only a quarter of the set's
    # height: the page keeps that much room above the set for them.
    for k, (length, r) in enumerate(((6.0, 0.42), (5.2, 0.33), (4.6, 0.25))):
        end = at + direction * length
        mid = (at + end) / 2
        bpy.ops.mesh.primitive_cylinder_add(vertices=32, radius=r, depth=length, location=mid)
        rod = bpy.context.active_object
        rod.rotation_euler = direction.to_track_quat("Z", "Y").to_euler()
        rod.data.materials.append(M["chrome"])
        for poly in rod.data.polygons:
            poly.use_smooth = True
        if k < 2:
            bpy.ops.mesh.primitive_cylinder_add(vertices=32, radius=r + 0.08, depth=0.5, location=end - direction * 0.25)
            collar = bpy.context.active_object
            collar.rotation_euler = rod.rotation_euler
            collar.data.materials.append(M["chrome"])
        at = end - direction * 0.3
    sphere(f"ear-ball{side}", at + direction * 1.05, 1.15, M["ball"])
    EARS.append(at + direction * 1.05)

# ---- The plinth ------------------------------------------------------------------------

# A lime bumper the set stands on, as the player's casing has.
prism("plinth", rrect(0, -PLINTH / 2, W - 3.0, PLINTH, PLINTH / 2 - 0.01, seg=8), YF + 1.0, D - 3.0, M["lime"], bev=0.25)

# Lift everything onto the plinth: z = 0 is the table.
LIFT = Matrix.Translation((0, 0, PLINTH))
bpy.context.view_layer.update()
for ob in list(scene.objects):
    if ob.parent is None:
        ob.matrix_world = LIFT @ ob.matrix_world


def placed(x, y, z):
    return LIFT @ Vector((x, y, z))


# ---- Floor, lights, camera -------------------------------------------------------

bpy.ops.mesh.primitive_plane_add(size=1, location=(0, 0, 0))
floor = bpy.context.active_object
floor.scale = (W + 60, D + 60, 1)
floor.is_shadow_catcher = True

world = bpy.data.worlds.new("world")
scene.world = world
try:
    world.use_nodes = True
except AttributeError:
    pass
env = world.node_tree.nodes.new("ShaderNodeTexEnvironment")
env.image = bpy.data.images.load(glob.glob("/usr/share/blender/*/datafiles/studiolights/world/studio.exr")[0])
bg = world.node_tree.nodes["Background"]
bg.inputs["Strength"].default_value = 0.45
world.node_tree.links.new(env.outputs["Color"], bg.inputs["Color"])

# radio.py's lights, three times as far and as big for a set three times
# the size (so nine times the power, for the same light on it).
SCALE = 3.0


def area(name, loc, power, size, color=(1, 1, 1), target=(0, 0, 7)):
    data = bpy.data.lights.new(name, "AREA")
    data.energy = power * SCALE * SCALE
    data.size = size * SCALE
    data.color = color
    ob = link(bpy.data.objects.new(name, data))
    ob.location = Vector(loc) * SCALE
    direction = Vector(target) * SCALE - ob.location
    ob.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()
    return ob


area("key", (-50, -40, 50), 55000, 45)
area("fill", (45, -40, 15), 22000, 40, color=(0.9, 0.95, 1.0))
area("rim", (25, 30, 60), 90000, 25, color=(1.0, 0.97, 0.92), target=(0, 0, 14))
area("violet", (-30, 40, 30), 40000, 20, color=(0.55, 0.3, 1.0), target=(0, 0, 8))

# Softboxes in front of the set, seen only in the glass: a window of light
# up and to the left, tall and leaning, and a dim wide one overhead, the
# soft reflections a tube's face catches in a product shot. They light
# nothing else. Where on the glass each lands comes from where the bowed
# face mirrors the space in front of it (aimed by tracing rays from the
# camera off it).


def softbox(name, loc, size, lean, strength):
    bpy.ops.mesh.primitive_plane_add(size=1, location=loc)
    ob = bpy.context.active_object
    ob.name = name
    ob.scale = (*size, 1)
    aim = (Vector((SX, YF, SZ + PLINTH)) - ob.location).to_track_quat("Z", "Y")
    ob.rotation_euler = (aim @ Matrix.Rotation(math.radians(lean), 4, "Z").to_quaternion()).to_euler()
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    nt = m.node_tree
    nt.nodes.remove(nt.nodes["Principled BSDF"])
    emit = nt.nodes.new("ShaderNodeEmission")
    # Brightest in the middle, falling off smoothly to its edges, as light
    # through a diffuser does.
    coords = nt.nodes.new("ShaderNodeTexCoord")
    grad = nt.nodes.new("ShaderNodeTexGradient")
    grad.gradient_type = "SPHERICAL"
    nt.links.new(coords.outputs["Object"], grad.inputs["Vector"])
    ramp = nt.nodes.new("ShaderNodeMapRange")
    ramp.interpolation_type = "SMOOTHSTEP"
    ramp.inputs["From Min"].default_value = 0.5
    ramp.inputs["From Max"].default_value = 0.9
    ramp.inputs["To Max"].default_value = strength
    nt.links.new(grad.outputs["Fac"], ramp.inputs["Value"])
    nt.links.new(ramp.outputs["Result"], emit.inputs["Strength"])
    nt.links.new(emit.outputs["Emission"], nt.nodes["Material Output"].inputs["Surface"])
    ob.data.materials.append(m)
    ob.visible_camera = False
    ob.visible_diffuse = False
    ob.visible_shadow = False
    ob.visible_transmission = False
    ob.visible_volume_scatter = False
    return ob


SOFTBOXES = [
    softbox("window", (-55.0, -170.0, 44.0), (16.0, 44.0), -24, 2.4),
    softbox("overhead", (-15.0, -170.0, 66.0), (130.0, 26.0), 0, 0.09),
]

# Looking straight along +y, from above and to the right: the camera isn't
# turned, the lens is shifted, so every plane facing the camera (the front
# and its screen) keeps its right angles.
cam_data = bpy.data.cameras.new("cam")
cam_data.lens = 100
cam_data.sensor_fit = "HORIZONTAL"
cam = link(bpy.data.objects.new("cam", cam_data))
scene.camera = cam
cam.location = (38.0, -560.0, PLINTH + H + 52.0)
cam.rotation_euler = (math.radians(90), 0, 0)

scene.render.resolution_x = 1000
scene.render.resolution_y = 1000
bpy.context.view_layer.update()
deps = bpy.context.evaluated_depsgraph_get()
corners = []
for ob in scene.objects:
    if ob.type in {"MESH", "CURVE", "FONT"} and ob is not floor and ob not in SOFTBOXES:
        ev = ob.evaluated_get(deps)
        corners += [ev.matrix_world @ Vector(c) for c in ev.bound_box]
uv = [world_to_camera_view(scene, cam, c) for c in corners]
u0, u1 = min(p.x for p in uv), max(p.x for p in uv)
v0, v1 = min(p.y for p in uv), max(p.y for p in uv)
MARGIN = 0.012
u0, u1, v0, v1 = u0 - MARGIN, u1 + MARGIN, v0 - MARGIN * 2.5, v1 + MARGIN
fw, fh = u1 - u0, v1 - v0
cam_data.lens = cam_data.lens / fw
cam_data.shift_x = ((u0 + u1) / 2 - 0.5) / fw
cam_data.shift_y = ((v0 + v1) / 2 - 0.5) / fw
scene.render.resolution_x = RES_X
scene.render.resolution_y = round(RES_X * fh / fw)
bpy.context.view_layer.update()
RX, RY = scene.render.resolution_x, scene.render.resolution_y

# ---- Render settings -------------------------------------------------------------

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
scene.cycles.max_bounces = 12
scene.cycles.caustics_refractive = False
scene.cycles.caustics_reflective = False
scene.render.film_transparent = True
scene.render.image_settings.file_format = "PNG"
scene.render.image_settings.color_mode = "RGBA"
scene.render.resolution_percentage = 100
# Standard, not AgX, as radio.py: these are toy plastics whose whole point
# is their color.
scene.view_settings.view_transform = "Standard"
scene.view_settings.exposure = -0.25


def px(p):
    """A world point's place in the image, in pixels from the top left."""
    c = world_to_camera_view(scene, cam, p)
    return (c.x * RX, (1 - c.y) * RY)


def box(cx, cz, rx, rz, y):
    """The pixel box (x, y, w, h) round a front-facing part centered at
    (cx, cz) and reaching rx, rz from it, at depth y."""
    (x0, y0), (x1, y1) = px(placed(cx - rx, y, cz + rz)), px(placed(cx + rx, y, cz - rz))
    return [math.floor(x0), math.floor(y0), math.ceil(x1 - x0), math.ceil(y1 - y0)]


def render(path, region=None):
    """Render to `path`, all of it or just the pixel box `region`."""
    scene.render.use_border = region is not None
    scene.render.use_crop_to_border = region is not None
    if region:
        x, y, w, h = region
        scene.render.border_min_x = x / RX
        scene.render.border_max_x = (x + w) / RX
        scene.render.border_max_y = 1 - y / RY
        scene.render.border_min_y = 1 - (y + h) / RY
    scene.render.filepath = path
    bpy.ops.render.render(write_still=True)


def load(path):
    image = bpy.data.images.load(path)
    w, h = image.size
    out = np.empty(w * h * 4, dtype=np.float32)
    image.pixels.foreach_get(out)
    bpy.data.images.remove(image)
    return out.reshape(h, w, 4)[::-1].copy()  # top row first


def save(path, pixels):
    h, w = pixels.shape[:2]
    image = bpy.data.images.new(os.path.basename(path), w, h, alpha=True)
    image.pixels.foreach_set(np.ascontiguousarray(pixels[::-1]).ravel())
    image.filepath_raw = path
    image.file_format = "PNG"
    image.save()
    bpy.data.images.remove(image)


def feathered(frame_px):
    """Fade a sprite frame's edges to transparent over FEATHER pixels."""
    h, w = frame_px.shape[:2]
    ys = np.minimum(np.arange(h), np.arange(h)[::-1])[:, None]
    xs = np.minimum(np.arange(w), np.arange(w)[::-1])[None, :]
    ramp = np.clip(np.minimum(ys, xs) / FEATHER, 0, 1)
    out = frame_px.copy()
    out[..., 3] *= ramp
    return out


def sprite(name, region, poses):
    """Render `region` once per pose (a function that sets the scene up),
    and stack the frames, feathered, into `name`.png."""
    frames = []
    for k, pose in enumerate(poses):
        pose()
        path = os.path.join(OUT_DIR, "frames", f"{name}-{k}.png")
        render(path, region)
        frames.append(feathered(load(path)))
    save(os.path.join(OUT_DIR, f"{name}.png"), np.concatenate(frames))


# ---- The still --------------------------------------------------------------------

glass.is_holdout = True
STILL = os.path.join(OUT_DIR, "tv.png")
render(STILL)

# radio.py's shadow: render the set alone for its coverage, take the rest
# of each pixel's alpha as shadow, and fade that with distance from the
# plinth's footprint, so the floor's far, faint shadow doesn't read as a box.
MASK = os.path.join(OUT_DIR, "frames", "mask.png")
floor.hide_render = True
samples = scene.cycles.samples
scene.cycles.samples = 32
render(MASK)
scene.cycles.samples = samples
floor.hide_render = False
still, mask = load(STILL), load(MASK)
foot = [px(Vector((x, y, 0))) for x, y in ((-W / 2 + 1.5, YF + 1), (W / 2 - 1.5, YF + 1), (W / 2 - 1.5, D / 2 - 2), (-W / 2 + 1.5, D / 2 - 2))]
ys, xs = np.mgrid[0:RY, 0:RX].astype(np.float32)
inside = np.ones_like(xs, dtype=bool)
dist = np.full_like(xs, np.inf)
turn = np.sign(sum(ax * by - bx * ay for (ax, ay), (bx, by) in zip(foot, foot[1:] + foot[:1])))
for (ax, ay), (bx, by) in zip(foot, foot[1:] + foot[:1]):
    ex, ey = bx - ax, by - ay
    cross = ex * (ys - ay) - ey * (xs - ax)
    t = np.clip(((xs - ax) * ex + (ys - ay) * ey) / (ex * ex + ey * ey), 0, 1)
    dist = np.minimum(dist, np.hypot(xs - (ax + t * ex), ys - (ay + t * ey)))
    inside &= cross * turn >= 0
dist = np.where(inside, 0, dist)
fade = np.exp(-((dist / (0.03 * RX)) ** 2))
m = mask[..., 3]
shadow = np.clip(still[..., 3] - m, 0, None)
# The glass's hole is see-through in both: keep it so.
still[..., 3] = m + np.minimum(shadow * fade, 0.7 * (1 - m))
save(STILL, still)

# ---- Where things are ------------------------------------------------------------

# The opening, at the depth of the tube's face: the page puts the picture
# there, and the gasket in tv.png covers its edge.
screen_tl = px(placed(SX - SW / 2, GLASS_Y, SZ + SH / 2))
screen_br = px(placed(SX + SW / 2, GLASS_Y, SZ - SH / 2))
front_tl = px(placed(-W / 2, YF, H))
front_br = px(placed(W / 2, YF, 0))
lcd_tl = px(placed(CX - LCD_W / 2, YP - 0.05, LCD_Z + LCD_H / 2))
lcd_br = px(placed(CX + LCD_W / 2, YP - 0.05, LCD_Z - LCD_H / 2))


def rect(tl, br):
    return [round(tl[0], 1), round(tl[1], 1), round(br[0] - tl[0], 1), round(br[1] - tl[1], 1)]


# What shows through the gasket: the camera, up and to the right, sees past
# the opening's near edge onto more of the face on that side, so the hole
# in the still is a little bigger than the opening. Measured from the
# still's alpha round where the face is.
x0, y0, x1, y1 = screen_tl[0], screen_tl[1], screen_br[0], screen_br[1]
pad = 0.04 * (x1 - x0)
window = still[int(y0 - pad) : int(y1 + pad), int(x0 - pad) : int(x1 + pad), 3] < 0.5
rows, cols = np.nonzero(window)
hole = [
    round(int(x0 - pad) + cols.min() - 1.0, 1),
    round(int(y0 - pad) + rows.min() - 1.0, 1),
    round(cols.max() - cols.min() + 3.0, 1),
    round(rows.max() - rows.min() + 3.0, 1),
]

layout = {
    "width": RX,
    "height": RY,
    # The front face, from the table up: what the page lays out by.
    "front": rect(front_tl, front_br),
    # The tube's face, where the page draws the picture: the hole in the
    # still, a pixel over all round, so no gap shows at its edge.
    "screen": hole,
    "screen_radius": round((px(placed(SX + SR, GLASS_Y, SZ))[0] - px(placed(SX, GLASS_Y, SZ))[0]), 1),
    "lcd": rect(lcd_tl, lcd_br),
}

# ---- Sprites -----------------------------------------------------------------------

TRAVEL = 0.45  # how far a button goes in when pressed


def press(ob, by):
    def pose():
        ob.location.y = ob["rest"] + by
    return pose


for name, (empty, (bx, bz, br)) in BUTTONS.items():
    empty["rest"] = empty.location.y
    region = box(bx, bz, br + 0.5, br + 0.5, YP - 1.0)
    sprite(f"tv-{name}", region, [press(empty, 0), press(empty, TRAVEL)])
    press(empty, 0)()
    layout[f"ch_{name}"] = region

power["rest"] = power.location.y
region = box(CX, PW_Z, PW_R + 0.6, PW_R + 0.6, YP - 1.5)
sprite("tv-power", region, [press(power, 0), press(power, TRAVEL * 1.6)])
press(power, 0)()
layout["power"] = region


def turn(degrees):
    def pose():
        # Clockwise as seen from the front: the knob faces -y.
        knob.rotation_euler = (0, math.radians(degrees), 0)
    return pose


region = box(CX, VOL_Z, VOL_R + 0.6, VOL_R + 0.6, YP - 2.0)
angles = [KNOB_FROM + (KNOB_TO - KNOB_FROM) * k / (KNOB_FRAMES - 1) for k in range(KNOB_FRAMES)]
sprite("tv-knob", region, [turn(a) for a in angles])
turn(0)()
layout["knob"] = {"box": region, "from": KNOB_FROM, "to": KNOB_TO, "frames": KNOB_FRAMES}


def light(mat):
    def pose():
        led.data.materials[0] = M[mat]
    return pose


region = box(LED_X, LED_Z, 1.0, 1.0, YP - 0.3)
sprite("tv-led", region, [light("led-off"), light("led-red"), light("led-green")])
light("led-off")()
layout["led"] = region

# ---- The glass's reflections ------------------------------------------------------------

# Everything but the glass is held out, so only the glass shows, and only
# where it isn't behind the gasket; it still reflects the set's own parts
# near its edges, and the studio.
glass.is_holdout = False
for ob in scene.objects:
    if ob.type in {"MESH", "CURVE", "FONT"} and ob not in (glass, floor, *SOFTBOXES):
        ob.is_holdout = True
floor.hide_render = True
# The studio's lamps and its bright ceiling would show in the glass as a
# hard white crescent where the face curves up; in their place, a dim even
# room, so only the softboxes, placed for it, catch the eye.
for ob in scene.objects:
    if ob.type == "LIGHT":
        ob.hide_render = True
for link_ in list(bg.inputs["Color"].links):
    world.node_tree.links.remove(link_)
bg.inputs["Color"].default_value = (0.03, 0.03, 0.036, 1)
bg.inputs["Strength"].default_value = 1.0
x, y, w, h = layout["screen"]
region = [math.floor(x) - 4, math.floor(y) - 4, math.ceil(w) + 8, math.ceil(h) + 8]
render(os.path.join(OUT_DIR, "tv-glass.png"), region)
layout["glass"] = region

with open(os.path.join(OUT_DIR, "tv.json"), "w") as f:
    json.dump(layout, f)
