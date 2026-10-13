"""Renders the side panel's clock radio in Blender (Cycles), after the 90s
Nickelodeon clock radio, turned a little to the viewer's right: an indigo
body with a tank-tread edge all round, a green ball on a purple post, a
clear fluorescent-lime acrylic tube, a red mushroom alarm dome, a spotted
lime speaker with an orange burst, a backlit LCD (left dark, the page lights
it), purple knobs, the alarm rocker and a crescent FM dial.

    blender -b -P assets/blender/radio.py -- /tmp/r/radio.png /tmp/r 512
    cwebp -q 90 -alpha_q 100 -m 6 /tmp/r/radio.png -o assets/radio.webp

The fonts dir needs LuckiestGuy.ttf and Fredoka.ttf (Blender can't read the
web fonts in assets/fonts/; get the TTFs from github.com/google/fonts).

Writes <out.png> (transparent, with a contact shadow) and <out>.json with
the image's size and where the LCD glass lands in it. The page draws its
live clock over the glass: copy `width`, `height` and `svg_matrix` into
RADIO_LCD and the radio's <img> in web.rs.
"""

import glob
import json
import math
import sys

import bmesh
import bpy
from bpy_extras.object_utils import world_to_camera_view
from mathutils import Matrix, Vector

argv = sys.argv[sys.argv.index("--") + 1 :]
OUT = argv[0]
FONTS = argv[1]
SAMPLES = int(argv[2]) if len(argv) > 2 else 256
RES_X = 960

bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene


def lin(hex_color):
    """sRGB hex to linear RGB."""
    out = []
    for i in (0, 2, 4):
        c = int(hex_color.lstrip("#")[i : i + 2], 16) / 255
        out.append(c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4)
    return out


def material(name, color, rough=0.35, coat=0.0, transmission=0.0, sss=0.0, ior=1.45, metallic=0.0):
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
    p.inputs["Transmission Weight"].default_value = transmission
    if sss:
        p.inputs["Subsurface Weight"].default_value = sss
        p.inputs["Subsurface Radius"].default_value = (0.6, 0.6, 0.6)
        p.inputs["Subsurface Scale"].default_value = 0.3
    return m


def plastic_grain(m, scale=900.0, strength=0.06):
    """Fine bump so molded plastic doesn't look like CG glass."""
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    noise = nt.nodes.new("ShaderNodeTexNoise")
    noise.inputs["Scale"].default_value = scale
    noise.inputs["Detail"].default_value = 4
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = strength
    bump.inputs["Distance"].default_value = 0.02
    nt.links.new(noise.outputs["Fac"], bump.inputs["Height"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    # Uneven gloss: handled plastic has smudgy patches, so its reflections
    # aren't perfectly even. Coarse noise, squashed so most of it is clean.
    smudge = nt.nodes.new("ShaderNodeTexNoise")
    smudge.inputs["Scale"].default_value = 0.9
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


def seam_groove(m, y):
    """The hairline where front and back shells meet, at local depth y: a
    narrow dark dip in the surface, done in the shader (a boolean cut spoils
    the bevel's shading)."""
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    tc = nt.nodes.new("ShaderNodeTexCoord")
    sep = nt.nodes.new("ShaderNodeSeparateXYZ")
    off = nt.nodes.new("ShaderNodeMath")
    off.operation = "SUBTRACT"
    off.inputs[1].default_value = y
    dist = nt.nodes.new("ShaderNodeMath")
    dist.operation = "ABSOLUTE"
    groove = nt.nodes.new("ShaderNodeMapRange")
    groove.inputs["From Min"].default_value = 0.025
    groove.inputs["From Max"].default_value = 0.06
    nt.links.new(tc.outputs["Object"], sep.inputs["Vector"])
    nt.links.new(sep.outputs["Y"], off.inputs[0])
    nt.links.new(off.outputs["Value"], dist.inputs[0])
    nt.links.new(dist.outputs["Value"], groove.inputs["Value"])
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = 1.0
    bump.inputs["Distance"].default_value = 0.04
    nt.links.new(groove.outputs["Result"], bump.inputs["Height"])
    grain = p.inputs["Normal"].links[0].from_socket
    nt.links.new(grain, bump.inputs["Normal"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    shade = nt.nodes.new("ShaderNodeMix")
    shade.data_type = "RGBA"
    shade.inputs["A"].default_value = (*lin("#0c0b1e"), 1)
    shade.inputs["B"].default_value = p.inputs["Base Color"].default_value
    nt.links.new(groove.outputs["Result"], shade.inputs["Factor"])
    nt.links.new(shade.outputs["Result"], p.inputs["Base Color"])


M = {
    "body": plastic_grain(material("body", "#1b1452", rough=0.42, coat=0.08)),
    "panel": plastic_grain(material("panel", "#221a63", rough=0.36, coat=0.2)),
    "purple": plastic_grain(material("purple", "#7426f2", rough=0.28, coat=0.4, sss=0.08)),
    "lime": plastic_grain(material("lime", "#8be01a", rough=0.3, coat=0.35, sss=0.15)),
    "ball": material("ball", "#82dc1c", rough=0.16, coat=0.7, sss=0.3),
    "red": material("red", "#e3231a", rough=0.12, coat=0.8, sss=0.35),
    "orange": plastic_grain(material("orange", "#ff5e00", rough=0.3, coat=0.45, sss=0.1)),
    "white": material("white", "#f7f7f2", rough=0.4),
    "ink": material("ink", "#120c1e", rough=0.5),
    "print": material("print", "#b6f23a", rough=0.5),
    "dark": material("dark", "#0d0b16", rough=0.6),
}


def lcd_material():
    """A backlit LCD switched to black: near-black green under a glossy
    cover. The page draws the lit digits over it."""
    m = material("lcd", "#040705", rough=0.5, coat=0.45)
    m.node_tree.nodes["Principled BSDF"].inputs["Coat Roughness"].default_value = 0.03
    return m


M["lcd"] = lcd_material()


def tube_material():
    """Clear fluorescent acrylic: almost all transmission, tinted lime, with
    the glow fluorescent plastic has where light gets trapped and comes out
    at the edges."""
    m = material("tube", "#eaff9a", rough=0.03, transmission=1.0, ior=1.49, coat=1.0)
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    p.inputs["Coat Roughness"].default_value = 0.02
    # It's a hollow tube: looking straight through, you cross two thin walls
    # and see what's behind, faintly yellow-lime; toward the edges you look
    # along the walls, through a lot of plastic, and see glowing green.
    weight = nt.nodes.new("ShaderNodeLayerWeight")
    weight.inputs["Blend"].default_value = 0.45
    tint = nt.nodes.new("ShaderNodeMix")
    tint.data_type = "RGBA"
    tint.inputs["A"].default_value = (*lin("#d8ff50"), 1)
    tint.inputs["B"].default_value = (*lin("#6edc00"), 1)
    nt.links.new(weight.outputs["Facing"], tint.inputs["Factor"])
    nt.links.new(tint.outputs["Result"], p.inputs["Emission Color"])
    glow = nt.nodes.new("ShaderNodeMapRange")
    glow.inputs["To Min"].default_value = 0.35
    glow.inputs["To Max"].default_value = 2.6
    nt.links.new(weight.outputs["Facing"], glow.inputs["Value"])
    nt.links.new(glow.outputs["Result"], p.inputs["Emission Strength"])
    vol = nt.nodes.new("ShaderNodeVolumeAbsorption")
    vol.inputs["Color"].default_value = (*lin("#c4ff2a"), 1)
    vol.inputs["Density"].default_value = 0.25
    nt.links.new(vol.outputs["Volume"], nt.nodes["Material Output"].inputs["Volume"])
    return m


M["tube"] = tube_material()


def spots_material():
    """The speaker's lime face with irregular black spots, slightly raised."""
    m = material("spots", "#8fe31c", rough=0.32, coat=0.35)
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    tc = nt.nodes.new("ShaderNodeTexCoord")
    vor = nt.nodes.new("ShaderNodeTexVoronoi")
    vor.inputs["Scale"].default_value = 1.8
    vor.inputs["Randomness"].default_value = 1.0
    nt.links.new(tc.outputs["Object"], vor.inputs["Vector"])
    # Each cell's random color sets its spot's size, so they vary.
    sep = nt.nodes.new("ShaderNodeSeparateColor")
    nt.links.new(vor.outputs["Color"], sep.inputs["Color"])
    size = nt.nodes.new("ShaderNodeMapRange")
    size.inputs["To Min"].default_value = 0.14
    size.inputs["To Max"].default_value = 0.3
    nt.links.new(sep.outputs["Red"], size.inputs["Value"])
    edge = nt.nodes.new("ShaderNodeMath")
    edge.operation = "SUBTRACT"
    nt.links.new(vor.outputs["Distance"], edge.inputs[0])
    nt.links.new(size.outputs["Result"], edge.inputs[1])
    spot = nt.nodes.new("ShaderNodeMapRange")
    spot.inputs["From Min"].default_value = 0.0
    spot.inputs["From Max"].default_value = 0.02
    nt.links.new(edge.outputs["Value"], spot.inputs["Value"])
    mix = nt.nodes.new("ShaderNodeMix")
    mix.data_type = "RGBA"
    mix.inputs["A"].default_value = (*lin("#0b0d06"), 1)
    mix.inputs["B"].default_value = (*lin("#8fe31c"), 1)
    nt.links.new(spot.outputs["Result"], mix.inputs["Factor"])
    nt.links.new(mix.outputs["Result"], p.inputs["Base Color"])
    inv = nt.nodes.new("ShaderNodeMath")
    inv.operation = "SUBTRACT"
    inv.inputs[0].default_value = 1.0
    nt.links.new(spot.outputs["Result"], inv.inputs[1])
    bump = nt.nodes.new("ShaderNodeBump")
    bump.inputs["Strength"].default_value = 0.4
    bump.inputs["Distance"].default_value = 0.02
    nt.links.new(inv.outputs["Value"], bump.inputs["Height"])
    nt.links.new(bump.outputs["Normal"], p.inputs["Normal"])
    return m


M["spots"] = spots_material()


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


def lathe(name, profile, cx, cy, z0, mat, segs=72, bev=0.05):
    """Spin a (radius, height) profile, bottom to top, about a vertical axis."""
    bm = bmesh.new()
    rings = []
    for r, h in profile:
        rings.append(
            [
                bm.verts.new((cx + r * math.cos(t), cy + r * math.sin(t), z0 + h))
                for t in (2 * math.pi * k / segs for k in range(segs))
            ]
        )
    for a, b in zip(rings, rings[1:]):
        for k in range(segs):
            m = (k + 1) % segs
            bm.faces.new((a[k], a[m], b[m], b[k]))
    bm.faces.new(list(reversed(rings[0])))
    bm.faces.new(rings[-1])
    ob = from_bmesh(name, bm, mat)
    return bevel(ob, bev, 3) if bev else ob


def sphere(name, loc, r, mat, scale=(1, 1, 1)):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=96, ring_count=48, radius=r, location=loc)
    ob = bpy.context.active_object
    ob.name = name
    ob.scale = scale
    for poly in ob.data.polygons:
        poly.use_smooth = True
    ob.data.materials.append(mat)
    return ob


def text(name, body, font, size, loc, mat, rot=0.0, extrude=0.015):
    cu = bpy.data.curves.new(name, "FONT")
    cu.body = body
    cu.font = bpy.data.fonts.load(font)
    cu.size = size
    cu.extrude = extrude
    cu.align_x = "CENTER"
    cu.align_y = "CENTER"
    ob = link(bpy.data.objects.new(name, cu))
    ob.location = loc
    ob.rotation_euler = (math.radians(90), math.radians(rot), 0)
    ob.data.materials.append(mat)
    return ob


# ---- Outlines ----------------------------------------------------------------


def rrect_walk(cx, cz, w, h, r):
    """Walk round a rounded rectangle by arc length, counterclockwise from
    the bottom of its right side: returns (length, at), where at(s) gives the
    point and outward normal (x, z, nx, nz)."""
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


def circle_walk(cx, cz, r):
    def at(s):
        ang = s / r
        return cx + r * math.cos(ang), cz + r * math.sin(ang), math.cos(ang), math.sin(ang)

    return 2 * math.pi * r, at


def toothed(walk, depth, period, shape=(0.0, 0.08, 0.86, 0.94)):
    """An outline with square-ish teeth standing out from it, like the
    tank-tread edge the radio has all round. `shape` is where each tooth
    rises, tops out and falls, as fractions of its period."""
    total, at = walk
    n = max(6, round(total / period))
    step = total / n
    pts = []
    for k in range(n):
        for frac, out in zip(shape, (0, 1, 1, 0)):
            x, z, nx, nz = at((k + frac) * step)
            pts.append((x + nx * depth * out, z + nz * depth * out))
    return pts, n


def resampled(walk, n, shape=(0.0, 0.08, 0.86, 0.94)):
    """The plain outline at the same points `toothed` uses, so the two can
    be joined by `frame`."""
    total, at = walk
    step = total / n
    return [at((k + frac) * step)[:2] for k in range(n) for frac in shape]


def burst(cx, cz, rx, rz, spikes, depth, rot):
    """A flame burst: an ellipse with sawtooth spikes round it, leaning one
    way, like a 90s logo splat."""
    pts = []
    for i in range(spikes * 2):
        lean = 0.35 if i % 2 == 0 else 0.0
        a = 2 * math.pi * (i + lean) / (spikes * 2)
        k = 1 + depth if i % 2 == 0 else 1 - depth * 0.35
        pts.append((math.cos(a) * rx * k, math.sin(a) * rz * k))
    c, s_ = math.cos(math.radians(rot)), math.sin(math.radians(rot))
    return [(cx + x * c - z * s_, cz + x * s_ + z * c) for x, z in pts]


def crescent(outer_c, outer_r, inner_c, inner_r, n=64):
    """The part of the outer circle outside the inner one, on its right."""
    (ox, oz), (ix, iz) = outer_c, inner_c
    d = math.dist(outer_c, inner_c)
    # Where the circles cross: a along the line between centers, h off it.
    a = (outer_r ** 2 - inner_r ** 2 + d ** 2) / (2 * d)
    h = math.sqrt(outer_r ** 2 - a ** 2)
    ux, uz = (ix - ox) / d, (iz - oz) / d
    top = (ox + a * ux - h * uz, oz + a * uz + h * ux)
    bottom = (ox + a * ux + h * uz, oz + a * uz - h * ux)
    if top[1] < bottom[1]:
        top, bottom = bottom, top

    def angle(c, p):
        return math.atan2(p[1] - c[1], p[0] - c[0])

    pts = []
    a0, a1 = angle(outer_c, bottom), angle(outer_c, top)
    for k in range(n + 1):
        t = a0 + (a1 - a0) * k / n
        pts.append((ox + outer_r * math.cos(t), oz + outer_r * math.sin(t)))
    b0, b1 = angle(inner_c, top), angle(inner_c, bottom)
    for k in range(1, n):
        t = b0 + (b1 - b0) * k / n
        pts.append((ix + inner_r * math.cos(t), iz + inner_r * math.sin(t)))
    return pts


def flower(cx, cz, r, lobes=6, n=96):
    pts = []
    for k in range(n):
        a = 2 * math.pi * k / n
        rr = r * (0.84 + 0.16 * math.cos(lobes * a))
        pts.append((cx + rr * math.cos(a), cz + rr * math.sin(a)))
    return pts


def filleted(points, radius, steps=10):
    """A polyline with each corner rounded to about `radius`: the bends of
    a heat-bent acrylic tube."""
    pts = [Vector(p) for p in points]
    out = [pts[0]]
    for p0, p1, p2 in zip(pts, pts[1:], pts[2:]):
        d1, d2 = (p0 - p1).normalized(), (p2 - p1).normalized()
        r = min(radius, (p0 - p1).length / 2, (p2 - p1).length / 2)
        a, b = p1 + d1 * r, p1 + d2 * r
        for k in range(steps + 1):
            t = k / steps
            out.append((1 - t) ** 2 * a + 2 * (1 - t) * t * p1 + t * t * b)
    out.append(pts[-1])
    return out


def arc_text(name, body, font, size, cx, cz, r, a0, a1, y, mat):
    """Letters set round an arc, from angle a0 to a1 (degrees)."""
    for k, ch in enumerate(body):
        t = math.radians(a0 + (a1 - a0) * (k + 0.5) / len(body))
        text(f"{name}{k}", ch, font, size, (cx + r * math.cos(t), y, cz + r * math.sin(t)), mat,
             rot=-(math.degrees(t) - 90), extrude=0.015)


def slot_knob(name, cx, cz, r, y0, depth, mat):
    """A round knob with a raised bar across its face, and a white pointer
    dot at the bar's end."""
    bpy.ops.mesh.primitive_cylinder_add(vertices=96, radius=r, depth=depth, location=(cx, y0 - depth / 2, cz), rotation=(math.radians(90), 0, 0))
    k = bpy.context.active_object
    k.name = name
    k.data.materials.append(mat)
    bevel(k, 0.12, 4)
    bpy.ops.mesh.primitive_cube_add(size=1, location=(cx, y0 - depth - 0.12, cz))
    bar = bpy.context.active_object
    bar.scale = (r * 1.7, 0.3, r * 0.38)
    bar.rotation_euler = (0, math.radians(-35), 0)
    bar.data.materials.append(mat)
    bevel(bar, 0.08, 3)
    bpy.ops.mesh.primitive_cylinder_add(vertices=24, radius=r * 0.1, depth=0.04, location=(cx - r * 0.66 * math.cos(math.radians(35)), y0 - depth - 0.01, cz + r * 0.66 * math.sin(math.radians(35))), rotation=(math.radians(90), 0, 0))
    bpy.context.active_object.data.materials.append(M["white"])


def cylinder_y(name, cx, cz, r, y0, depth, mat, bev=0.05, verts=96):
    """A disc facing the camera, from y0 back by `depth`."""
    bpy.ops.mesh.primitive_cylinder_add(vertices=verts, radius=r, depth=depth, location=(cx, y0 + depth / 2, cz), rotation=(math.radians(90), 0, 0))
    ob = bpy.context.active_object
    ob.name = name
    ob.data.materials.append(mat)
    if bev:
        bevel(ob, bev, 3)
    return ob


# ---- Body --------------------------------------------------------------------
# Units are centimeters. The body sits on z = 0, its front face toward -y.
# Positions come from measuring the reference photo (about 38 px to the cm).

W, H, D = 21.0, 9.4, 7.0
R = 3.9
TEETH = 0.26
YF = -D / 2  # front face of the shell

# The shell: its whole outline is tread, wide segments split by grooves,
# which run front to back over the top and round the ends.
shell = M["body"].copy()
seam_groove(shell, YF + D * 0.62)
tread, _ = toothed(rrect_walk(0, H / 2, W, H, R), TEETH, 1.55)
prism("body", tread, YF, D, shell, bev=0.07, segs=3)
# A smooth core under the teeth, so the grooves have a floor.
prism("core", rrect(0, H / 2, W - 0.02, H - 0.02, R - 0.01, seg=32), YF + 0.15, D - 0.3, shell, bev=0.3, segs=4)

# The front: a bright purple trim just inside the teeth, round a raised
# panel.
TRIM = 0.32
frame("trim", rrect(0, H / 2, W - 0.1, H - 0.1, R - 0.05, seg=24), rrect(0, H / 2, W - 0.9, H - 0.9, R - 0.45, seg=24), YF - 0.22, 0.3, M["purple"], bev=0.08)
prism("front", rrect(0, H / 2, W - 0.9, H - 0.9, R - 0.45, seg=24), YF - 0.12, 0.2, M["panel"], bev=0.08, segs=3)
YP = YF - 0.12  # front of the panel

TOP = H + TEETH  # top of the tread

# ---- Front: speaker ---------------------------------------------------------------

SX, SZ, SR = -6.45, 4.55, 3.0
teeth, n = toothed(circle_walk(SX, SZ, SR + 0.28), 0.22, 0.75)
frame("speaker-ring", teeth, resampled(circle_walk(SX, SZ, SR - 0.05), n), YP - 0.3, 0.34, M["purple"], bev=0.05)
cylinder_y("grille", SX, SZ, SR, YP - 0.08, 0.2, M["spots"], bev=0.02)
burst_pts = burst(SX + 0.1, SZ - 0.05, 2.45, 1.25, 11, 0.22, 14)
prism("burst-shadow", burst_pts, YP - 0.2, 0.12, M["ink"], bev=0.02, segs=2).location = (0.06, 0, -0.08)
prism("burst", burst_pts, YP - 0.32, 0.12, M["orange"], bev=0.04, segs=3)
# White lettering with a black outline, as printed.
text("joy-edge", "JOY!", f"{FONTS}/LuckiestGuy.ttf", 1.2, (SX + 0.1, YP - 0.36, SZ - 0.05), M["ink"], rot=-14, extrude=0.02).data.offset = 0.06
text("joy", "JOY!", f"{FONTS}/LuckiestGuy.ttf", 1.2, (SX + 0.1, YP - 0.42, SZ - 0.05), M["white"], rot=-14, extrude=0.02)

# ---- Front: LCD -------------------------------------------------------------------

LX, LZ, LW, LH = 1.45, 6.6, 5.5, 2.5
frame(
    "lcd-bezel",
    rrect(LX, LZ, LW + 0.9, LH + 0.9, 0.5, seg=8),
    rrect(LX, LZ, LW, LH, 0.2, seg=8),
    YP - 0.3,
    0.36,
    M["purple"],
)
bpy.ops.mesh.primitive_plane_add(size=1, location=(LX, YP - 0.04, LZ), rotation=(math.radians(90), 0, 0))
lcd = bpy.context.active_object
lcd.name = "lcd"
lcd.scale = (LW, LH, 1)
lcd.data.materials.append(M["lcd"])
text("lcd-fm", "FM", f"{FONTS}/Fredoka.ttf", 0.26, (LX - LW / 2 + 0.3, YP - 0.06, LZ + LH / 2 - 0.25), M["print"])
# The little purple button by the LCD's corner.
cylinder_y("snooze", -2.55, 7.55, 0.5, YP - 0.45, 0.5, M["purple"], bev=0.12)

# ---- Front: knobs and the alarm switch ------------------------------------------------

VX, VZ = -1.55, 3.3
slot_knob("volume", VX, VZ, 0.82, YP, 0.55, M["purple"])
arc_text("volume-label", "VOLUME", f"{FONTS}/LuckiestGuy.ttf", 0.5, VX, VZ, 1.25, 160, 25, YP - 0.01, M["print"])
TX, TZ = -1.95, 1.55
slot_knob("tune", TX, TZ, 0.72, YP, 0.5, M["purple"])
text("tune-label", "TUNE", f"{FONTS}/Fredoka.ttf", 0.26, (TX - 1.15, YP - 0.01, TZ - 0.25), M["print"])

SWX, SWZ, SWW, SWH = 2.45, 2.55, 3.3, 2.75
frame("switch-frame", rrect(SWX, SWZ, SWW, SWH, 0.3, seg=6), rrect(SWX, SWZ, SWW - 0.6, SWH - 0.6, 0.15, seg=6), YP - 0.26, 0.3, M["lime"])
bpy.ops.mesh.primitive_plane_add(size=1, location=(SWX, YP - 0.01, SWZ), rotation=(math.radians(90), 0, 0))
well = bpy.context.active_object
well.scale = (SWW - 0.6, SWH - 0.6, 1)
well.data.materials.append(M["body"])
text("alarm-on", "ALARM\nON", f"{FONTS}/Fredoka.ttf", 0.22, (SWX - 0.75, YP - 0.03, SWZ + 0.55), M["print"])
text("alarm-off", "ALARM\nOFF", f"{FONTS}/Fredoka.ttf", 0.22, (SWX - 0.75, YP - 0.03, SWZ - 0.55), M["print"])
# The rocker: a purple drum with a lime arrow pointing at ON.
prism("rocker", rrect(SWX + 0.6, SWZ, 0.95, SWH - 0.85, 0.3, seg=6), YP - 0.62, 0.6, M["purple"], bev=0.16, segs=4)
prism("arrow", [(SWX + 0.15, SWZ + 0.35), (SWX + 0.55, SWZ + 0.6), (SWX + 0.55, SWZ + 0.47), (SWX + 0.95, SWZ + 0.47), (SWX + 0.95, SWZ + 0.23), (SWX + 0.55, SWZ + 0.23), (SWX + 0.55, SWZ + 0.1)], YP - 0.7, 0.1, M["lime"], bev=0.02, segs=2)

# ---- Front: the FM dial -----------------------------------------------------------------

# A lime crescent window following the rounded end, framed in purple, with
# a six-lobed tuning knob at its inner edge.
EX = W / 2 - R  # center of the right end's curve
crescent_pts = crescent((EX, H / 2), 3.35, (EX - 2.1, H / 2), 3.35)
prism("dial-frame", crescent((EX, H / 2), 3.6, (EX - 2.5, H / 2), 3.6), YP - 0.16, 0.2, M["purple"], bev=0.06)
prism("dial", crescent_pts, YP - 0.2, 0.08, M["lime"], bev=0.02, segs=2)
for k, label in enumerate(["88", "92", "96", "100", "104"]):
    a = math.radians(62 - k * 31)
    text(f"fm{label}", label, f"{FONTS}/LuckiestGuy.ttf", 0.42, (EX + 2.45 * math.cos(a), YP - 0.25, H / 2 + 2.45 * math.sin(a)), M["ink"], rot=-math.degrees(a) * 0.6)
for k in range(25):
    a = math.radians(70 - k * 5.8)
    bpy.ops.mesh.primitive_cube_add(size=1, location=(EX + 3.08 * math.cos(a), YP - 0.25, H / 2 + 3.08 * math.sin(a)))
    tick = bpy.context.active_object
    tick.scale = (0.34 if k % 4 == 0 else 0.18, 0.03, 0.045)
    tick.rotation_euler = (0, -a, 0)
    tick.data.materials.append(M["ink"])
KX, KZ = EX - 0.55, H / 2 - 0.1
prism("tuner", flower(KX, KZ, 1.35), YP - 0.95, 0.95, M["purple"], bev=0.1, segs=4)
cylinder_y("tuner-hub", KX, KZ, 0.6, YP - 1.1, 0.2, M["purple"], bev=0.08)

# ---- Top: ball post, alarm dome, tube -------------------------------------------------------

PX = -6.05
lathe(
    "post",
    [(1.2, 0), (1.2, 0.3), (0.95, 0.42), (0.62, 0.5), (0.58, 0.95), (0.95, 1.05), (0.95, 1.35), (0.6, 1.45), (0.56, 1.85), (0.9, 1.95), (0.9, 2.25), (0.5, 2.35)],
    PX,
    0,
    TOP - 0.1,
    M["purple"],
)
BALL = Vector((PX, 0, TOP + 3.8))
sphere("ball", BALL, 1.15, M["ball"])
# The collar the tube leaves the ball through.
bpy.ops.mesh.primitive_cylinder_add(vertices=48, radius=0.42, depth=1.0, location=(PX + 1.4, 0, BALL.z - 0.4), rotation=(0, math.radians(90), 0))
collar = bpy.context.active_object
collar.data.materials.append(M["purple"])
bevel(collar, 0.08, 3)

DOMEX = 5.7
lathe("dome-base", [(1.9, 0), (1.9, 0.75), (1.6, 0.95), (1.2, 1.0)], DOMEX, 0, TOP - 0.1, M["purple"], bev=0.12)
lathe("dome-collar", [(1.15, 0), (1.15, 0.35), (1.0, 0.45)], DOMEX, 0, TOP + 0.85, M["lime"])
DOME = Vector((DOMEX, 0, TOP + 1.25))
# A squat mushroom cap: the top half of a squashed sphere, with a little lip
# at its rim. (Its open underside faces away from the camera.)
dome = sphere("dome", DOME, 2.05, M["red"], scale=(1, 1, 0.82))
bm = bmesh.new()
bm.from_mesh(dome.data)
bmesh.ops.delete(bm, geom=[v for v in bm.verts if v.co.z < -1e-4], context="VERTS")
bm.to_mesh(dome.data)
bm.free()
lathe("dome-lip", [(1.95, 0), (2.12, 0.06), (2.1, 0.2), (1.95, 0.24)], DOMEX, 0, DOME.z - 0.12, M["red"], bev=0.04)

# The tube: heat-bent clear lime acrylic, leaving the ball's collar level,
# zigzagging, and dropping into a fitting on the top.
tube_path = filleted(
    [
        (PX + 1.9, 0, BALL.z - 0.4),
        (PX + 2.55, 0, BALL.z - 0.4),
        (-1.85, 0, TOP + 5.0),
        (-0.4, 0, TOP + 2.1),
        (0.9, 0, TOP + 4.75),
        (2.4, 0, TOP + 1.3),
        (3.0, 0, TOP + 0.1),
    ],
    0.5,
)
cu = bpy.data.curves.new("tube", "CURVE")
cu.dimensions = "3D"
cu.bevel_depth = 0.31
cu.bevel_resolution = 8
cu.use_fill_caps = True
sp = cu.splines.new("POLY")
sp.points.add(len(tube_path) - 1)
for p, co in zip(sp.points, tube_path):
    p.co = (*co, 1)
tube = link(bpy.data.objects.new("tube", cu))
tube.data.materials.append(M["tube"])
lathe("tube-fitting", [(0.55, 0), (0.55, 0.45), (0.42, 0.55)], 3.0, 0, TOP - 0.1, M["purple"])

# Rubber feet, which lift it a little off the table.
FOOT = 0.3
for fx in (-W / 2 + 3.2, W / 2 - 3.2):
    for fy in (-D / 2 + 1.2, D / 2 - 1.2):
        bpy.ops.mesh.primitive_cylinder_add(vertices=48, radius=0.8, depth=FOOT + 0.2, location=(fx, fy, -FOOT / 2 + 0.1))
        bpy.context.active_object.data.materials.append(M["dark"])

# Turn it a little: the front faces 15 degrees to the viewer's right (5:30
# on a clock seen from above, with 6 facing the camera), showing a sliver of
# the ball's end. Everything built so far is the radio.
PLACE = Matrix.Rotation(math.radians(15), 4, "Z") @ Matrix.Translation((0, 0, FOOT))
bpy.context.view_layer.update()
for ob in list(scene.objects):
    if ob.parent is None:
        ob.matrix_world = PLACE @ ob.matrix_world

# ---- Floor, lights, camera -------------------------------------------------------------

# Only around the radio: the floor further back shows above it in the frame.
bpy.ops.mesh.primitive_plane_add(size=1, location=(0, 0, 0))
floor = bpy.context.active_object
floor.scale = (W + 20, W + 20, 1)
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
# Through the clear tube you'd see what's behind it on the page: the side
# panel's blue. Rays that passed through glass see that instead of the
# studio, and the tube renders opaque with the blue inside it.
behind = world.node_tree.nodes.new("ShaderNodeBackground")
behind.inputs["Color"].default_value = (*lin("#2f68dc"), 1)
behind.inputs["Strength"].default_value = 1.0
path = world.node_tree.nodes.new("ShaderNodeLightPath")
pick = world.node_tree.nodes.new("ShaderNodeMixShader")
world.node_tree.links.new(path.outputs["Is Transmission Ray"], pick.inputs["Fac"])
world.node_tree.links.new(bg.outputs["Background"], pick.inputs[1])
world.node_tree.links.new(behind.outputs["Background"], pick.inputs[2])
world.node_tree.links.new(pick.outputs["Shader"], world.node_tree.nodes["World Output"].inputs["Surface"])

def area(name, loc, power, size, color=(1, 1, 1), target=(0, 0, 7)):
    data = bpy.data.lights.new(name, "AREA")
    data.energy = power
    data.size = size
    data.color = color
    ob = link(bpy.data.objects.new(name, data))
    ob.location = loc
    direction = Vector(target) - Vector(loc)
    ob.rotation_euler = direction.to_track_quat("-Z", "Y").to_euler()
    return ob


area("key", (-50, -40, 50), 55000, 45)
area("fill", (45, -40, 15), 22000, 40, color=(0.9, 0.95, 1.0))
# Back lights sit off to the sides or high up: straight behind, the flat
# tops of the plates would mirror them into the camera.
area("rim", (25, 30, 60), 90000, 25, color=(1.0, 0.97, 0.92), target=(0, 0, 14))
area("tube-left", (-40, 25, 22), 25000, 15, target=(0, 0, 16))
area("tube-right", (40, 25, 22), 25000, 15, target=(0, 0, 16))
# A violet kicker from behind, like the blacklight in the reference photo.
area("violet", (-30, 40, 30), 40000, 20, color=(0.55, 0.3, 1.0), target=(0, 0, 8))

cam_data = bpy.data.cameras.new("cam")
cam_data.lens = 100
cam_data.sensor_fit = "HORIZONTAL"
cam = link(bpy.data.objects.new("cam", cam_data))
scene.camera = cam
ELEV = math.radians(21)
DIST = 120
TARGET = Vector((0, 0, 8.0))
cam.location = TARGET + Vector((0, -DIST * math.cos(ELEV), DIST * math.sin(ELEV)))
cam.rotation_euler = (TARGET - cam.location).to_track_quat("-Z", "Y").to_euler()

# Frame the radio and its shadow tightly: measure where its bounds land on a
# square frame, then narrow the lens and shift to just that region.
scene.render.resolution_x = 1000
scene.render.resolution_y = 1000
bpy.context.view_layer.update()
deps = bpy.context.evaluated_depsgraph_get()
corners = []
for ob in scene.objects:
    if ob.type in {"MESH", "CURVE", "FONT"} and ob is not floor:
        ev = ob.evaluated_get(deps)
        corners += [ev.matrix_world @ Vector(c) for c in ev.bound_box]
corners += [PLACE @ Vector((x, y, -FOOT)) for x in (-W / 2 - 1.5, W / 2 + 1.5) for y in (-D / 2 - 1.5, D / 2 + 1.5)]
uv = [world_to_camera_view(scene, cam, c) for c in corners]
u0, u1 = min(p.x for p in uv), max(p.x for p in uv)
v0, v1 = min(p.y for p in uv), max(p.y for p in uv)
MARGIN = 0.012
u0, u1, v0, v1 = u0 - MARGIN, u1 + MARGIN, v0 - MARGIN, v1 + MARGIN
fw, fh = u1 - u0, v1 - v0
cam_data.lens = cam_data.lens / fw
cam_data.shift_x = ((u0 + u1) / 2 - 0.5) / fw
cam_data.shift_y = ((v0 + v1) / 2 - 0.5) / fw
scene.render.resolution_x = RES_X
scene.render.resolution_y = round(RES_X * fh / fw)
bpy.context.view_layer.update()

# A little depth of field, focused on the LCD, as a product photo has. The
# scene is in centimeters but Blender takes them for meters, so the f-stop
# is a hundredth of the real one (f/8 here).
LCD_CENTER = PLACE @ Vector((LX, YP - 0.04, LZ))
cam_data.dof.use_dof = True
cam_data.dof.focus_distance = (cam.location - LCD_CENTER).length
cam_data.dof.aperture_fstop = 0.08

# ---- Render ---------------------------------------------------------------------------------

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
scene.cycles.max_bounces = 24
scene.cycles.transmission_bounces = 24
scene.cycles.transparent_max_bounces = 24
scene.cycles.caustics_refractive = False
scene.cycles.caustics_reflective = False
scene.render.film_transparent = True

scene.render.image_settings.file_format = "PNG"
scene.render.image_settings.color_mode = "RGBA"
scene.render.resolution_percentage = 100
# Standard, not AgX: AgX turns saturated purples lavender, and these are
# toy plastics whose whole point is their color.
scene.view_settings.view_transform = "Standard"
scene.view_settings.exposure = -0.25
scene.render.filepath = OUT

# Where the LCD glass lands in the image, top-left origin, clockwise from
# the top-left corner.
lcd_px = []
for dx, dz in ((-0.5, 0.5), (0.5, 0.5), (0.5, -0.5), (-0.5, -0.5)):
    p = world_to_camera_view(scene, cam, PLACE @ Vector((LX + dx * LW, YP - 0.04, LZ + dz * LH)))
    lcd_px.append(Vector((p.x * scene.render.resolution_x, (1 - p.y) * scene.render.resolution_y)))
# The page draws the clock in a 100-wide box and maps it onto the glass with
# an SVG matrix(a b c d e f). That's affine, which a long lens's view of a
# small flat panel very nearly is; `error` says how far off the fourth
# corner lands, in pixels.
LCD_UNITS = (100, 100 * LH / LW)
top_left, top_right, bottom_right, bottom_left = lcd_px
across = (top_right - top_left) / LCD_UNITS[0]
down = (bottom_left - top_left) / LCD_UNITS[1]
predicted = top_left + across * LCD_UNITS[0] + down * LCD_UNITS[1]
with open(OUT.rsplit(".", 1)[0] + ".json", "w") as f:
    json.dump(
        {
            "width": scene.render.resolution_x,
            "height": scene.render.resolution_y,
            "lcd": [[round(c, 2) for c in p] for p in lcd_px],
            "lcd_units": [round(u, 3) for u in LCD_UNITS],
            "svg_matrix": [round(v, 5) for v in (across.x, across.y, down.x, down.y, top_left.x, top_left.y)],
            "error": round((predicted - bottom_right).length, 2),
        },
        f,
    )

bpy.ops.render.render(write_still=True)

# Big soft lights give the floor a faint shadow far out to its edges, which
# on the page reads as a box. Keep the contact shadow: render the radio
# alone for its coverage m, take the rest of each pixel's alpha as shadow,
# and fade that with the distance from the footprint.
import numpy as np

MASK = OUT.rsplit(".", 1)[0] + "-mask.png"
floor.hide_render = True
scene.cycles.samples = 32
scene.render.filepath = MASK
bpy.ops.render.render(write_still=True)

rx, ry = scene.render.resolution_x, scene.render.resolution_y
# The footprint's outline in the image, in order round it.
foot = [
    world_to_camera_view(scene, cam, PLACE @ Vector((x, y, -FOOT)))
    for x, y in ((-W / 2, -D / 2), (W / 2, -D / 2), (W / 2, D / 2), (-W / 2, D / 2))
]
foot = [(p.x * rx, (1 - p.y) * ry) for p in foot]


def pixels(path):
    image = bpy.data.images.load(path)
    out = np.empty(rx * ry * 4, dtype=np.float32)
    image.pixels.foreach_get(out)
    return image, out.reshape(ry, rx, 4)  # bottom row first


img, px = pixels(OUT)
_, mask = pixels(MASK)
ys, xs = np.mgrid[0:ry, 0:rx]
y_down = (ry - 1 - ys).astype(np.float32)
xs = xs.astype(np.float32)
# Distance from each pixel to the footprint (0 inside it).
inside = np.ones_like(xs, dtype=bool)
dist = np.full_like(xs, np.inf)
# Which way the outline winds: inside is on the same side of every edge.
turn = np.sign(sum(ax * by - bx * ay for (ax, ay), (bx, by) in zip(foot, foot[1:] + foot[:1])))
for (ax, ay), (bx, by) in zip(foot, foot[1:] + foot[:1]):
    ex, ey = bx - ax, by - ay
    cross = ex * (y_down - ay) - ey * (xs - ax)
    t = np.clip(((xs - ax) * ex + (y_down - ay) * ey) / (ex * ex + ey * ey), 0, 1)
    dist = np.minimum(dist, np.hypot(xs - (ax + t * ex), y_down - (ay + t * ey)))
    inside &= cross * turn >= 0
dist = np.where(inside, 0, dist)
fade = np.exp(-((dist / (0.03 * rx)) ** 2))
m = mask[..., 3]
shadow = np.clip(px[..., 3] - m, 0, None)
px[..., 3] = m + np.minimum(shadow * fade, 0.7 * (1 - m))
img.pixels.foreach_set(px.ravel())
img.filepath_raw = OUT
img.file_format = "PNG"
img.save()
