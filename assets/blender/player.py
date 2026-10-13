"""Renders the video page's hardware in the clock radio's plastic and light
(radio.py, badges.py): the Download now dome and the player's casing and
controls. Everything but the domes faces the camera, as the badges do.

- dome-download, dome-play (.png): the radio's red mushroom dome on its
  purple base, seen from above as it sits on the page, with DOWNLOAD NOW or
  a play triangle printed on its cap. Two frames stacked: up, then pressed.
- casing.png: the player's purple casing on its lime bumper, with the
  screen's hole and the tray the controls sit in. A nine-slice image.
- panel.png: the tray alone on a purple slab, for fullscreen. Nine-slice.
- button-violet, button-red (.png): round push buttons in their sockets, up
  then pressed. The page prints Video.js's icons on them.
- groove.png: the slot a slider runs in. Nine-slice.
- fader.png, ball.png: the seek and volume thumbs.
- lcd.png: a dark LCD in a purple bezel. Nine-slice.
- led.png: a slideshow dot's LED, off then lit.

    blender -b -P assets/blender/player.py -- /tmp/r /path/to/fonts [samples]
    for f in /tmp/r/*.png; do cwebp -q 90 -alpha_q 100 -m 6 $f -o assets/$(basename ${f%.png}).webp; done

The fonts dir needs LuckiestGuy.ttf (see radio.py). Sizes are in CSS px,
UNIT world units to the px, and each image is drawn at S px to the CSS px;
player.css's numbers (slices, insets, the screen's place in the casing)
come from the constants here, so change both together.
"""

import glob
import math
import sys

import bmesh
import bpy
import numpy as np
from mathutils import Vector

argv = sys.argv[sys.argv.index("--") + 1 :]
OUT_DIR = argv[0]
FONTS = argv[1]
SAMPLES = int(argv[2]) if len(argv) > 2 else 512

UNIT = 0.1  # world units per CSS px

bpy.ops.wm.read_factory_settings(use_empty=True)
scene = bpy.context.scene


def lin(hex_color):
    """sRGB hex to linear RGB."""
    out = []
    for i in (0, 2, 4):
        c = int(hex_color.lstrip("#")[i : i + 2], 16) / 255
        out.append(c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4)
    return out


def plastic(name, color, rough=0.3, coat=0.45, sss=0.1, grain=300.0, smudge_scale=0.3):
    """badges.py's molded plastic: a clear coat, a little light scattering
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
    smudge.inputs["Scale"].default_value = smudge_scale
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


def glossy(name, color, rough=0.12, coat=0.8, sss=0.35):
    """radio.py's dome and ball: smooth, deep, wet-looking plastic."""
    m = bpy.data.materials.new(name)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    p = m.node_tree.nodes["Principled BSDF"]
    p.inputs["Base Color"].default_value = (*lin(color), 1)
    p.inputs["Roughness"].default_value = rough
    p.inputs["Coat Weight"].default_value = coat
    p.inputs["Coat Roughness"].default_value = 0.08
    p.inputs["Subsurface Weight"].default_value = sss
    p.inputs["Subsurface Radius"].default_value = (0.6, 0.6, 0.6)
    p.inputs["Subsurface Scale"].default_value = 0.3
    return m


def lcd_material():
    """radio.py's LCD switched off: near-black green under a glossy cover.
    The page lights the digits."""
    m = glossy("lcd", "#040705", rough=0.5, coat=0.3, sss=0)
    m.node_tree.nodes["Principled BSDF"].inputs["Coat Roughness"].default_value = 0.03
    return m


def led_material(name, color, glow):
    """An LED's clear lens over its die: glossy, with the die's light, if
    lit, coming through."""
    m = glossy(name, color, rough=0.08, coat=0.9, sss=0.5)
    p = m.node_tree.nodes["Principled BSDF"]
    if glow:
        p.inputs["Emission Color"].default_value = (*lin(color), 1)
        p.inputs["Emission Strength"].default_value = glow
    return m


M = {
    # The casing is a broad face; coarser smudges read as handling, not
    # blotches, when the nine-slice stretches it.
    "purple": plastic("purple", "#5b2fc4", rough=0.3, coat=0.45, sss=0.08, smudge_scale=0.08),
    "tray": plastic("tray", "#241252", rough=0.45, coat=0.15, sss=0, smudge_scale=0.08),
    "gasket": plastic("gasket", "#120a26", rough=0.55, coat=0.1, sss=0),
    "lime": plastic("lime", "#8be01a", rough=0.3, coat=0.35, sss=0.15, smudge_scale=0.08),
    "violet": plastic("violet", "#8f5cf5", rough=0.25, coat=0.6, sss=0.15),
    "socket": plastic("socket", "#1a0d3d", rough=0.4, coat=0.2, sss=0),
    "red": glossy("red", "#e3231a"),
    "redcap": plastic("redcap", "#e3231a", rough=0.2, coat=0.7, sss=0.3),
    "ball": glossy("ball", "#82dc1c", rough=0.16, coat=0.7, sss=0.3),
    "fader": plastic("fader", "#f2efe6", rough=0.3, coat=0.4, sss=0.15),
    "slot": plastic("slot", "#0b0618", rough=0.6, coat=0, sss=0),
    "lcd": lcd_material(),
    "led-off": led_material("led-off", "#aab2c4", 0),
    "led-on": led_material("led-on", "#a6e52b", 2.5),
}


# ---- Geometry helpers ------------------------------------------------------------------


def link(ob, group):
    for c in ob.users_collection:
        c.objects.unlink(ob)
    group.objects.link(ob)
    return ob


def group(name):
    c = bpy.data.collections.new(name)
    scene.collection.children.link(c)
    return c


def from_bmesh(name, bm, mat, grp):
    me = bpy.data.meshes.new(name)
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    bm.to_mesh(me)
    bm.free()
    me.materials.append(mat)
    for poly in me.polygons:
        poly.use_smooth = True
    ob = bpy.data.objects.new(name, me)
    scene.collection.objects.link(ob)
    return link(ob, grp)


def bevel(ob, width, segments=5, angle=30):
    mod = ob.modifiers.new("bevel", "BEVEL")
    mod.width = width
    mod.segments = segments
    mod.limit_method = "ANGLE"
    mod.angle_limit = math.radians(angle)
    mod.harden_normals = True
    return ob


def rrect(cx, cz, w, h, r, seg=16):
    """A rounded rectangle's outline (x, z), counterclockwise."""
    pts = []
    for x, z, a0 in ((cx + w / 2 - r, cz + h / 2 - r, 0), (cx - w / 2 + r, cz + h / 2 - r, 90),
                     (cx - w / 2 + r, cz - h / 2 + r, 180), (cx + w / 2 - r, cz - h / 2 + r, 270)):
        for i in range(seg + 1):
            a = math.radians(a0 + 90 * i / seg)
            pts.append((x + r * math.cos(a), z + r * math.sin(a)))
    return pts


def prism(name, pts, y0, depth, mat, grp, bev=0.12, segs=5):
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
    ob = from_bmesh(name, bm, mat, grp)
    return bevel(ob, bev, segs) if bev else ob


def frame(name, outer, inner, y0, depth, mat, grp, bev=0.06, segs=3):
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
    ob = from_bmesh(name, bm, mat, grp)
    return bevel(ob, bev, segs) if bev else ob


def cut(ob, cutter):
    """Subtract `cutter` from `ob` and hide the cutter. Bevel after the
    cuts, so their edges are rounded too."""
    mod = ob.modifiers.new("cut", "BOOLEAN")
    mod.operation = "DIFFERENCE"
    mod.solver = "EXACT"
    mod.object = cutter
    cutter.hide_render = True
    return ob


def lathe(name, profile, mat, grp, z0=0.0, segs=96, bev=0.05, facing=False):
    """Spin a (radius, height) profile, bottom to top, about the z axis.
    `facing` turns it to point its top at the camera (-y)."""
    bm = bmesh.new()
    rings = []
    for r, h in profile:
        rings.append([bm.verts.new((r * math.cos(t), r * math.sin(t), z0 + h)) for t in (2 * math.pi * k / segs for k in range(segs))])
    for a, b in zip(rings, rings[1:]):
        for k in range(segs):
            m = (k + 1) % segs
            bm.faces.new((a[k], a[m], b[m], b[k]))
    bm.faces.new(list(reversed(rings[0])))
    if profile[-1][0] > 1e-6:
        bm.faces.new(rings[-1])
    ob = from_bmesh(name, bm, mat, grp)
    if facing:
        ob.rotation_euler = (math.radians(90), 0, 0)
    return bevel(ob, bev, 3) if bev else ob


def sphere(name, loc, r, mat, grp, scale=(1, 1, 1)):
    bpy.ops.mesh.primitive_uv_sphere_add(segments=96, ring_count=48, radius=r, location=loc)
    ob = bpy.context.active_object
    ob.name = name
    ob.scale = scale
    for poly in ob.data.polygons:
        poly.use_smooth = True
    ob.data.materials.append(mat)
    return link(ob, grp)


def catcher(grp, y=None, z=None):
    """A plane behind (y) or under (z) a piece that keeps only its shadow."""
    if y is not None:
        bpy.ops.mesh.primitive_plane_add(size=400, location=(0, y, 0), rotation=(math.radians(90), 0, 0))
    else:
        bpy.ops.mesh.primitive_plane_add(size=400, location=(0, 0, z))
    ob = link(bpy.context.active_object, grp)
    ob.is_shadow_catcher = True
    return ob


def px(x, y, w, h):
    """CSS px in a w x h box (top-left origin, y down) to world (x, z) about
    the box's center."""
    return ((x - w / 2) * UNIT, (h / 2 - y) * UNIT)


def css_rrect(x, y, w, h, r, box_w, box_h, seg=16):
    cx, cz = px(x + w / 2, y + h / 2, box_w, box_h)
    return rrect(cx, cz, w * UNIT, h * UNIT, r * UNIT, seg)


# ---- The casing ---------------------------------------------------------------------------
# A W x H casing, in CSS px. The screen sits PAD in from the top and sides
# and STRIP above the bottom; under it, the tray runs the casing's width
# with TRAY_TOP above and TRAY_BOTTOM below it. player.css lays the page's
# player out to these.

PAD = 18
STRIP = 72
TRAY_TOP = 8
TRAY_BOTTOM = 10
SCREEN_R = 14
CASE_R = 30
GASKET = 3
# The image runs this far past the casing all round, for the bumper and,
# on the fullscreen panel, the shadow (border-image-outset in player.css).
MARGIN = 28


def shell(name, w, h, screen, grp, shadow):
    """A purple casing w x h with the tray across its bottom and, if
    `screen`, a hole for the screen above it, on a lime bumper, with its
    shadow if `shadow`."""
    tray_y = h - STRIP + TRAY_TOP if screen else TRAY_TOP
    tray_h = STRIP - TRAY_TOP - TRAY_BOTTOM
    # The bumper: a little wider and taller than the casing, behind it, so
    # it shows as a rim along the bottom. It sits behind the tray's floor,
    # and the screen goes through it.
    bumper = prism(f"{name}-bumper", css_rrect(-3, 5, w + 6, h + 6, CASE_R + 3, w, h, 24), -0.5, 0.7, M["lime"], grp, bev=0)
    body = prism(f"{name}-body", css_rrect(0, 0, w, h, CASE_R, w, h, 24), -1.2, 1.2, M["purple"], grp, bev=0)
    # The tray: a pill-shaped well through the casing, onto a dark floor.
    tray = prism(f"{name}-tray-cut", css_rrect(PAD, tray_y, w - 2 * PAD, tray_h, tray_h / 2, w, h, 24), -2, 3, M["purple"], grp, bev=0)
    cut(body, tray)
    prism(f"{name}-tray", css_rrect(PAD - 4, tray_y - 4, w - 2 * PAD + 8, tray_h + 8, tray_h / 2 + 4, w, h, 24), -0.55, 0.6, M["tray"], grp, bev=0)
    if screen:
        hole = css_rrect(PAD - GASKET, PAD - GASKET, w - 2 * (PAD - GASKET), h - PAD - STRIP + 2 * GASKET, SCREEN_R + GASKET, w, h, 16)
        cutter = prism(f"{name}-screen-cut", hole, -2, 3, M["purple"], grp, bev=0)
        cut(body, cutter)
        cut(bumper, cutter)
        # A dark gasket round the screen, set back from the bezel.
        frame(
            f"{name}-gasket",
            hole,
            css_rrect(PAD, PAD, w - 2 * PAD, h - PAD - STRIP, SCREEN_R, w, h, 16),
            -0.6,
            0.6,
            M["gasket"],
            grp,
            bev=0.08,
        )
    bevel(body, 0.5, 6)
    bevel(bumper, 0.45, 6)
    if shadow:
        catcher(grp, y=0.5)


CASING_W, CASING_H = 960, 640
CASING = group("casing")
# No shadow under the casing: on the page it ran into the window's edge
# and was cut off at the bottom.
shell("casing", CASING_W, CASING_H, True, CASING, False)

PANEL_W = 640
PANEL = group("panel")
shell("panel", PANEL_W, STRIP, False, PANEL, True)


# ---- Buttons -------------------------------------------------------------------------------
# Each is built round the origin facing the camera; BUTTON is the cap's
# diameter, the size of the page's button. The socket rings it a little
# wider.

BUTTON = 40
PRESS = 0.14  # how far a pressed cap sinks, world units


def push_button(name, cap_mat):
    grp = group(name)
    r = BUTTON / 2 * UNIT
    lathe(f"{name}-socket", [(r + 0.35, 0), (r + 0.35, 0.18), (r + 0.18, 0.3), (r + 0.06, 0.26), (r + 0.06, 0)], M["socket"], grp, bev=0.04, facing=True)
    cap = lathe(f"{name}-cap", [(r, 0), (r, 0.42), (r * 0.9, 0.56), (r * 0.6, 0.64), (0, 0.67)], cap_mat, grp, z0=0.05, bev=0.06, facing=True)
    catcher(grp, y=0.05)
    return grp, [cap]


BUTTONS = [push_button("button-violet", M["violet"]), push_button("button-red", M["redcap"])]


# ---- Domes ---------------------------------------------------------------------------------
# radio.py's alarm dome: a purple base, a lime collar, and a squat red
# mushroom cap with a lip, printed with a pad-printed mask (see `print_mask`).


def print_mask(name, build):
    """Renders the cap's print in a scene of its own, top down: the ink in
    red, and a dark offset copy for its drop shadow in green. Returns the
    image."""
    sc = bpy.data.scenes.new(f"print-{name}")
    sc.render.engine = "CYCLES"
    sc.cycles.samples = 16
    sc.cycles.use_denoising = False
    sc.render.resolution_x = sc.render.resolution_y = 1024
    sc.view_settings.view_transform = "Standard"
    sc.render.image_settings.file_format = "PNG"
    sc.render.filepath = f"{OUT_DIR}/print-{name}.png"
    sc.world = bpy.data.worlds.new(f"print-{name}")
    try:
        sc.world.use_nodes = True
    except AttributeError:
        pass
    sc.world.node_tree.nodes["Background"].inputs["Strength"].default_value = 0

    def ink(color):
        m = bpy.data.materials.new(f"ink-{color}")
        try:
            m.use_nodes = True
        except AttributeError:
            pass
        nt = m.node_tree
        nt.nodes.remove(nt.nodes["Principled BSDF"])
        em = nt.nodes.new("ShaderNodeEmission")
        em.inputs["Color"].default_value = (*color, 1)
        nt.links.new(em.outputs["Emission"], nt.nodes["Material Output"].inputs["Surface"])
        return m

    for ob, shadow in build():
        for c in ob.users_collection:
            c.objects.unlink(ob)
        sc.collection.objects.link(ob)
        ob.data.materials.clear()
        ob.data.materials.append(ink((0, 1, 0) if shadow else (1, 0, 0)))
    cam_data = bpy.data.cameras.new(f"print-{name}")
    cam_data.type = "ORTHO"
    cam_data.ortho_scale = 2.0
    cam = bpy.data.objects.new(f"print-{name}", cam_data)
    sc.collection.objects.link(cam)
    cam.location = (0, 0, 10)
    sc.camera = cam
    bpy.ops.render.render(write_still=True, scene=sc.name)
    img = bpy.data.images.load(sc.render.filepath)
    img.colorspace_settings.name = "Non-Color"
    return img


def lettering(body, size, y, shadow):
    cu = bpy.data.curves.new("lettering", "FONT")
    cu.body = body
    cu.font = bpy.data.fonts.load(f"{FONTS}/LuckiestGuy.ttf")
    cu.size = size
    cu.space_line = 0.82
    cu.align_x = "CENTER"
    cu.align_y = "CENTER"
    ob = bpy.data.objects.new("lettering", cu)
    scene.collection.objects.link(ob)
    # The shadow drops down and right, as the page's lettering's does.
    ob.location = (0.035, y - 0.05, 0) if shadow else (0, y, 0.1)
    return ob


# Prints sit a little toward the camera (-y) of the cap's top, to look
# centered on the part of it the camera sees.
def download_print():
    return [(lettering("DOWNLOAD\nNOW", 0.36, -0.15, s), s) for s in (True, False)]


def play_print():
    def triangle(shadow):
        # Pointing right, each corner an arc of radius r whose center sits
        # 2r in from the corner (the corners are 60 degrees).
        size, r = 0.62, 0.1
        pts = []
        for corner in (0, 120, 240):
            for k in range(13):
                a = math.radians(corner - 60 + 10 * k)
                c = math.radians(corner)
                pts.append(((size - 2 * r) * math.cos(c) + r * math.cos(a), (size - 2 * r) * math.sin(c) + r * math.sin(a), 0))
        bm = bmesh.new()
        bm.faces.new([bm.verts.new(p) for p in pts])
        me = bpy.data.meshes.new("triangle")
        bm.to_mesh(me)
        bm.free()
        ob = bpy.data.objects.new("triangle", me)
        scene.collection.objects.link(ob)
        ob.location = (0.11, -0.3, 0) if shadow else (0.08, -0.26, 0.1)
        return ob

    return [(triangle(s), s) for s in (True, False)]


def printed(name, mask, radius):
    """The cap's red with `mask` printed on it, projected straight down
    over its `radius`: white ink with a dark red drop shadow."""
    m = M["red"].copy()
    m.name = name
    nt = m.node_tree
    p = nt.nodes["Principled BSDF"]
    tc = nt.nodes.new("ShaderNodeTexCoord")
    mapping = nt.nodes.new("ShaderNodeMapping")
    mapping.inputs["Location"].default_value = (0.5, 0.5, 0)
    mapping.inputs["Scale"].default_value = (1 / (2 * radius), 1 / (2 * radius), 0)
    tex = nt.nodes.new("ShaderNodeTexImage")
    tex.image = mask
    tex.extension = "CLIP"
    sep = nt.nodes.new("ShaderNodeSeparateColor")
    nt.links.new(tc.outputs["Object"], mapping.inputs["Vector"])
    nt.links.new(mapping.outputs["Vector"], tex.inputs["Vector"])
    nt.links.new(tex.outputs["Color"], sep.inputs["Color"])
    shade = nt.nodes.new("ShaderNodeMix")
    shade.data_type = "RGBA"
    shade.inputs["A"].default_value = p.inputs["Base Color"].default_value
    shade.inputs["B"].default_value = (*lin("#6e0b06"), 1)
    nt.links.new(sep.outputs["Green"], shade.inputs["Factor"])
    ink = nt.nodes.new("ShaderNodeMix")
    ink.data_type = "RGBA"
    ink.inputs["B"].default_value = (*lin("#fffaf0"), 1)
    nt.links.new(shade.outputs["Result"], ink.inputs["A"])
    nt.links.new(sep.outputs["Red"], ink.inputs["Factor"])
    nt.links.new(ink.outputs["Result"], p.inputs["Base Color"])
    # Ink is a little less glossy and scatters no light into the plastic.
    rough = nt.nodes.new("ShaderNodeMapRange")
    rough.inputs["To Min"].default_value = p.inputs["Roughness"].default_value
    rough.inputs["To Max"].default_value = 0.35
    nt.links.new(sep.outputs["Red"], rough.inputs["Value"])
    nt.links.new(rough.outputs["Result"], p.inputs["Roughness"])
    sss = nt.nodes.new("ShaderNodeMapRange")
    sss.inputs["To Min"].default_value = p.inputs["Subsurface Weight"].default_value
    sss.inputs["To Max"].default_value = 0.0
    nt.links.new(sep.outputs["Red"], sss.inputs["Value"])
    nt.links.new(sss.outputs["Result"], p.inputs["Subsurface Weight"])
    return m


DOME_R = 2.05
DOME_PRESS = 0.22


def dome(name, cap_mat):
    grp = group(name)
    lathe(f"{name}-base", [(1.9, 0), (1.9, 0.75), (1.6, 0.95), (1.2, 1.0)], M["purple"], grp, bev=0.12)
    lathe(f"{name}-collar", [(1.15, 0), (1.15, 0.35), (1.0, 0.45)], M["lime"], grp, z0=0.95)
    # A squat mushroom cap: the top half of a squashed sphere, with a little
    # lip at its rim.
    cap = sphere(f"{name}-cap", (0, 0, 1.35), DOME_R, cap_mat, grp, scale=(1, 1, 0.82))
    bm = bmesh.new()
    bm.from_mesh(cap.data)
    bmesh.ops.delete(bm, geom=[v for v in bm.verts if v.co.z < -1e-4], context="VERTS")
    bm.to_mesh(cap.data)
    bm.free()
    lip = lathe(f"{name}-lip", [(1.95, 0), (2.12, 0.06), (2.1, 0.2), (1.95, 0.24)], M["red"], grp, z0=1.23, bev=0.04)
    catcher(grp, z=0)
    return grp, [cap, lip]


DOMES = [
    dome("dome-download", printed("dome-download", print_mask("download", download_print), DOME_R)),
    dome("dome-play", printed("dome-play", print_mask("play", play_print), DOME_R)),
]
# The domes are seen from in front and above, as they sit on the page.
DOME_TILT = 52


# ---- Sliders, LCD, LED ------------------------------------------------------------------------

# The slot: a dark channel with a purple rim, GROOVE_H tall.
GROOVE_W, GROOVE_H, GROOVE_RIM = 200, 16, 3
GROOVE = group("groove")
frame(
    "groove-rim",
    css_rrect(0, 0, GROOVE_W, GROOVE_H, GROOVE_H / 2, GROOVE_W, GROOVE_H),
    css_rrect(GROOVE_RIM, GROOVE_RIM, GROOVE_W - 2 * GROOVE_RIM, GROOVE_H - 2 * GROOVE_RIM, GROOVE_H / 2 - GROOVE_RIM, GROOVE_W, GROOVE_H),
    -0.22,
    0.22,
    M["purple"],
    GROOVE,
    bev=0.07,
)
prism("groove-floor", css_rrect(1, 1, GROOVE_W - 2, GROOVE_H - 2, GROOVE_H / 2 - 1, GROOVE_W, GROOVE_H), 0.0, 0.1, M["slot"], GROOVE, bev=0)
catcher(GROOVE, y=0.12)

# The seek thumb: a fader cap, with a lime line down its middle.
FADER_W, FADER_H = 14, 26
FADER = group("fader")
prism("fader", css_rrect(0, 0, FADER_W, FADER_H, 4, FADER_W, FADER_H), -0.6, 0.6, M["fader"], FADER, bev=0.22, segs=6)
for i, y in enumerate((5, 8, 18, 21)):
    prism(f"fader-rib{i}", css_rrect(3, y - 0.6, FADER_W - 6, 1.2, 0.6, FADER_W, FADER_H), -0.66, 0.1, M["fader"], FADER, bev=0.03, segs=2)
prism("fader-line", css_rrect(FADER_W / 2 - 1.2, 11, 2.4, 4, 1.2, FADER_W, FADER_H), -0.64, 0.1, M["lime"], FADER, bev=0.03, segs=2)
catcher(FADER, y=0.05)

# The volume thumb: the radio's lime ball.
BALL_D = 18
BALL = group("ball")
sphere("ball", (0, -BALL_D / 2 * UNIT, 0), BALL_D / 2 * UNIT, M["ball"], BALL)
catcher(BALL, y=0.05)

# The LCD: radio.py's dark glass in a purple bezel.
LCD_W, LCD_H, LCD_BEZEL = 140, 34, 4
LCD = group("lcd")
frame(
    "lcd-bezel",
    css_rrect(0, 0, LCD_W, LCD_H, 8, LCD_W, LCD_H),
    css_rrect(LCD_BEZEL, LCD_BEZEL, LCD_W - 2 * LCD_BEZEL, LCD_H - 2 * LCD_BEZEL, 4, LCD_W, LCD_H),
    -0.35,
    0.4,
    M["purple"],
    LCD,
    bev=0.1,
)
prism("lcd-glass", css_rrect(LCD_BEZEL - 1, LCD_BEZEL - 1, LCD_W - 2 * LCD_BEZEL + 2, LCD_H - 2 * LCD_BEZEL + 2, 5, LCD_W, LCD_H), -0.12, 0.1, M["lcd"], LCD, bev=0)
catcher(LCD, y=0.1)

# The LED: a lens in a dark ring, LED_D across.
LED_D = 12
LED = group("led")
r = LED_D / 2 * UNIT
lathe("led-ring", [(r + 0.22, 0), (r + 0.22, 0.12), (r + 0.1, 0.18), (r, 0.12), (r, 0)], M["socket"], LED, bev=0.03, facing=True)
LED_LENS = lathe("led-lens", [(r, 0), (r, 0.12), (r * 0.8, 0.3), (r * 0.4, 0.38), (0, 0.4)], M["led-off"], LED, z0=0.02, bev=0.03, facing=True)
catcher(LED, y=0.05)


# ---- Light: badges.py's studio --------------------------------------------------------------

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
# The studio lights reflections only, so the shadow catchers don't record
# its occlusion as a haze.
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
    return ob


# Key from the upper left, so shadows fall down and right like the
# splat's; a soft fill; a violet kicker like the radio's.
KEY = area("key", (-25, -30, 35), 42000, 30)
# Only the key casts shadows on the catchers: the fill's, thrown left and
# along the domes' floor, read as a grey haze round everything.
pieces = bpy.data.collections.new("pieces")
for ob in scene.objects:
    if ob.type in {"MESH", "CURVE"} and not ob.is_shadow_catcher:
        pieces.objects.link(ob)
VIOLET = area("violet", (20, 10, 25), 6000, 15, color=(0.55, 0.3, 1.0))
for light in (area("fill", (30, -35, 0), 9000, 30, color=(0.9, 0.95, 1.0)), VIOLET):
    light.light_linking.receiver_collection = pieces

cam_data = bpy.data.cameras.new("cam")
cam_data.type = "ORTHO"
cam_data.sensor_fit = "HORIZONTAL"
cam = bpy.data.objects.new("cam", cam_data)
scene.collection.objects.link(cam)
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
scene.view_settings.exposure = -0.15


def render(only, name, w, h, scale, ortho=None, tilt=0, center=(0, 0)):
    """Renders the group `only` into a w x h CSS px image at `scale` px to
    the CSS px, centered on `center` (x, z). The view is w CSS px across
    unless `ortho` (world units) says otherwise. With a tilt, the camera
    looks down from that many degrees above."""
    for c in scene.collection.children:
        c.hide_render = c is not only
    a = math.radians(tilt)
    cam.location = (center[0], -50 * math.cos(a), center[1] + 50 * math.sin(a))
    cam.rotation_euler = (math.radians(90 - tilt), 0, 0)
    cam_data.ortho_scale = ortho or w * UNIT
    scene.render.resolution_x = round(w * scale)
    scene.render.resolution_y = round(h * scale)
    scene.render.filepath = f"{OUT_DIR}/{name}.png"
    bpy.ops.render.render(write_still=True)
    return clean_shadow(scene.render.filepath)


def clean_shadow(path):
    """The catchers leave faint specks far from the piece: keep its shadow,
    cap how dark it gets, and drop the faintest. Returns the pixels."""
    img = bpy.data.images.load(path)
    px_ = np.empty(len(img.pixels), dtype=np.float32)
    img.pixels.foreach_get(px_)
    px_ = px_.reshape(-1, 4)
    # The PNG's pixels come back in bytes over 255, and the catchers' noise
    # leaves some shadow at 1/255 rather than black.
    shadow = px_[:, :3].max(axis=1) < 1.5 / 255
    px_ = px_.reshape(img.size[1], img.size[0], 4)
    shadow = shadow.reshape(img.size[1], img.size[0])
    # The catchers' shadows come out grainy: smooth them, over shadow
    # pixels only, so the piece's own edges don't bleed into them.
    alpha = np.where(shadow, px_[..., 3], 0)
    weight = blur(shadow.astype(np.float32), 3)
    smooth = blur(alpha, 3) / np.maximum(weight, 1e-6)
    px_[..., 3] = np.where(shadow, np.where(smooth < 0.06, 0, np.minimum(smooth, 0.45)), px_[..., 3])
    bpy.data.images.remove(img)
    return px_


def blur(a, r):
    """Three box blurs of radius r, near enough a Gaussian."""
    for _ in range(3):
        for axis in (0, 1):
            pad = [(0, 0), (0, 0)]
            pad[axis] = (r + 1, r)
            c = np.cumsum(np.pad(a, pad, mode="edge"), axis=axis)
            a = (np.take(c, range(2 * r + 1, c.shape[axis]), axis=axis) - np.take(c, range(0, c.shape[axis] - 2 * r - 1), axis=axis)) / (2 * r + 1)
    return a


def save(name, pixels):
    """Writes pixels (rows bottom up, as Blender keeps them) to name.png."""
    h, w, _ = pixels.shape
    img = bpy.data.images.new(name, w, h, alpha=True)
    img.pixels.foreach_set(pixels.ravel())
    img.filepath_raw = f"{OUT_DIR}/{name}.png"
    img.file_format = "PNG"
    img.save()


def two_frames(grp, name, moving, offset, w, h, scale, **kw):
    """Renders `grp` as it is, then with `moving` shifted by `offset`, and
    stacks the two, the first on top."""
    up = render(grp, name, w, h, scale, **kw)
    for ob in moving:
        ob.location = Vector(ob.location) + Vector(offset)
    down = render(grp, name, w, h, scale, **kw)
    for ob in moving:
        ob.location = Vector(ob.location) - Vector(offset)
    # Rows run bottom up, so the second frame goes first.
    save(name, np.concatenate([down, up]))


save("casing", render(CASING, "casing", CASING_W + 2 * MARGIN, CASING_H + 2 * MARGIN, 2))
save("panel", render(PANEL, "panel", PANEL_W + 2 * MARGIN, STRIP + 2 * MARGIN, 2))

# Buttons: the frame is 1.5 times the cap, so the page draws it 25% past
# the button on every side.
for grp, caps in BUTTONS:
    two_frames(grp, grp.name, caps, (0, PRESS, 0), BUTTON * 1.5, BUTTON * 1.5, 4)

save("groove", render(GROOVE, "groove", GROOVE_W + 8, GROOVE_H + 8, 4))
save("fader", render(FADER, "fader", FADER_W + 12, FADER_H + 12, 4))
save("ball", render(BALL, "ball", BALL_D + 12, BALL_D + 12, 4))
save("lcd", render(LCD, "lcd", LCD_W + 8, LCD_H + 8, 4))

off = render(LED, "led", LED_D * 2, LED_D * 2, 4)
LED_LENS.data.materials[0] = M["led-on"]
on = render(LED, "led", LED_D * 2, LED_D * 2, 4)
save("led", np.concatenate([on, off]))

# Domes: DOME_FRAME CSS px square, the cap a little above the middle so its
# base and shadow fit. The page sizes the button to the frame.
# The key moves higher for them, which shortens the shadow they throw back
# across the floor to fit the frame.
DOME_FRAME = 150
KEY.location = (-18, -22, 55)
KEY.rotation_euler = (-KEY.location).to_track_quat("-Z", "Y").to_euler()
# The violet kicker drops behind them, so its reflection rims the cap's
# right edge instead of landing on the print.
VIOLET.location = (30, 30, 6)
VIOLET.rotation_euler = (-VIOLET.location).to_track_quat("-Z", "Y").to_euler()
for grp, caps in DOMES:
    two_frames(grp, grp.name, caps, (0, 0, -DOME_PRESS), DOME_FRAME, DOME_FRAME, 4, ortho=5.8, tilt=DOME_TILT, center=(0.1, 1.0))
