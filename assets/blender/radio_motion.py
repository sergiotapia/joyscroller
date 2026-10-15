"""Renders the clock radio's moving parts for the side panel's hover
animation, from the scene radio.py builds, with the same camera and light,
so every layer lines up with the still and with each other:

- dome.png: the red alarm cap's poses, a sprite sheet: at rest, squashed
  into its collar, and lifted up its plunger (stretched a little, most
  near the bottom, where a bouncing cap moves fastest).
- knob.png: the six-lobed tuning knob turned through 60 degrees, which
  brings it back to where it started, so the frames loop.
- lit.png: the radio as it looks with the tube switched on, glowing, and
  its light falling on the body, the ball and the dome; dome-lit.png is
  the dome's poses in that light. The page fades these in with the hover.
- motion.json: where each layer goes in the still, the sheets' layout, and
  the tube's centerline in pixels, which the page's electricity follows.

The still's pixels under the dome's rectangle are cut out (the cap leaves
the background showing as it lifts), so the dome layer always draws them.
Cycles renders the same scene to the same pixels, so a layer's edges meet
the still without a seam.

Run through radio.py, which passes its globals: see its docstring.
"""

import json
import math
import os

import bpy
import numpy as np
from bpy_extras.object_utils import world_to_camera_view
from mathutils import Matrix, Vector

# How far the cap lifts at most, in cm, and in how many steps; the squashed
# poses, from barely to fully.
LIFT = 1.3
LIFTS = 24
SQUASHES = 5
# The knob's frames over one 60-degree turn.
TURNS = 15
# Blank pixels between sprite-sheet tiles, filled by repeating each tile's
# edge, so a scaled sheet never samples its neighbor.
GUTTER = 2
# A pixel counts as changed when a frame moves it by more than this (out of
# 255). The denoiser looks at a wide neighborhood, so moving one part
# shifts pixels everywhere by a level or two; those stay in the still.
SAME = 6
# Each layer fades into the still over this many pixels at its edges, so the
# smaller changes cut off there don't leave a line.
FEATHER = 12


def render(g, out_dir):
    os.makedirs(os.path.join(out_dir, "frames"), exist_ok=True)
    scene, cam, PLACE = g["scene"], g["cam"], g["PLACE"]
    rx, ry = scene.render.resolution_x, scene.render.resolution_y
    obs = bpy.data.objects

    floor, fade = g["floor"], g["fade"][::-1]
    covers = {}

    def shot(name, pose):
        """Render to <out_dir>/frames/<name>.png and return its pixels, top
        row first, as 0-255 integers, with the still's contact shadow. That
        needs the radio's coverage in this pose, rendered without the floor
        the first time a pose is shot (as radio.py does for the still)."""
        path = os.path.join(out_dir, "frames", name + ".png")
        scene.render.filepath = path
        bpy.ops.render.render(write_still=True)
        frame = load(path, rx, ry)
        if pose not in covers:
            floor.hide_render = True
            scene.cycles.samples = 32
            scene.render.filepath = os.path.join(out_dir, "frames", name + "-mask.png")
            bpy.ops.render.render(write_still=True)
            covers[pose] = load(scene.render.filepath, rx, ry)[..., 3]
            floor.hide_render = False
            scene.cycles.samples = g["SAMPLES"]
        frame[..., 3] = settle(frame[..., 3], covers[pose], fade)
        return frame

    # ---- Poses ---------------------------------------------------------------

    cap = [obs["dome"], obs["dome-lip"], obs["dome-stem"]]
    cap_rest = {ob: ob.matrix_world.copy() for ob in cap}
    pivot = Vector((g["DOMEX"], 0, g["DOME"].z - 0.12))

    def pose_dome(lift, sz=1.0):
        """Lift the cap by `lift` cm and scale it by `sz` upright about its
        rim, keeping its volume."""
        sxy = 1 / math.sqrt(sz)
        local = Matrix.Translation(pivot + Vector((0, 0, lift))) @ Matrix.Diagonal((sxy, sxy, sz, 1)) @ Matrix.Translation(-pivot)
        for ob in cap:
            ob.matrix_world = PLACE @ local @ PLACE.inverted() @ cap_rest[ob]

    dome_poses = [(0.0, 1.0)]
    for k in range(1, SQUASHES + 1):
        q = k / SQUASHES
        dome_poses.append((-0.12 * q, 1 - 0.2 * q))
    for k in range(1, LIFTS + 1):
        h = LIFT * k / LIFTS
        dome_poses.append((h, 1 + 0.06 * math.sqrt(1 - h / LIFT)))

    knob = [obs["tuner"], obs["tuner-hub"]]
    knob_rest = {ob: ob.matrix_world.copy() for ob in knob}
    axis = Vector((g["KX"], 0, g["KZ"]))

    def pose_knob(degrees):
        # The knob faces -y; turning it clockwise as seen from the front.
        local = Matrix.Translation(axis) @ Matrix.Rotation(math.radians(degrees), 4, "Y") @ Matrix.Translation(-axis)
        for ob in knob:
            ob.matrix_world = PLACE @ local @ PLACE.inverted() @ knob_rest[ob]

    # ---- Renders --------------------------------------------------------------

    rest = shot("rest", "rest")
    dome = []
    for i, (lift, sz) in enumerate(dome_poses):
        pose_dome(lift, sz)
        dome.append(rest if i == 0 else shot(f"dome-{i:02}", f"dome-{i}"))
    pose_dome(0.0)
    turns = [rest]
    for k in range(1, TURNS + 1):
        pose_knob(60 * k / TURNS)
        turns.append(shot(f"knob-{k:02}", f"knob-{k}"))
    pose_knob(0)
    # The last turn is 60 degrees, where the knob looks as it started: how
    # far it is from the first is how well the frames loop.
    turns_end = turns.pop()

    light_on(g)
    lit = shot("lit", "rest")
    dome_lit = [lit]
    for i, (lift, sz) in enumerate(dome_poses[1:], 1):
        pose_dome(lift, sz)
        dome_lit.append(shot(f"dome-lit-{i:02}", f"dome-{i}"))
    pose_dome(0.0)

    # ---- Layers ---------------------------------------------------------------

    def px(p):
        c = world_to_camera_view(scene, cam, p)
        return [round(c.x * rx, 1), round((1 - c.y) * ry, 1)]

    dome_box = changed([(rest, f) for f in dome[1:]] + [(lit, f) for f in dome_lit[1:]], px(PLACE @ g["DOME"]))
    knob_box = changed([(rest, f) for f in turns[1:]], px(PLACE @ Vector((g["KX"], g["YP"], g["KZ"]))))
    loop_error = int(np.percentile(np.abs(crop(turns_end, knob_box).astype(int) - crop(rest, knob_box).astype(int)), 99.9))

    # Inside a layer's box, the still keeps only what's opaque in every
    # frame, and the layer draws the rest: a see-through pixel (the
    # background, a shadow, an edge) laid over the same pixel would count
    # twice. That includes where a lifted cap uncovers what's behind it.
    still = load(g["OUT"], rx, ry)
    hole = np.zeros(rest.shape[:2], dtype=bool)
    for box, frames in ((dome_box, dome), (knob_box, turns)):
        x, y, w, h = box
        part = still[y : y + h, x : x + w, 3] < 255
        for f in frames:
            part |= crop(f, box)[..., 3] < 255
        hole[y : y + h, x : x + w] |= grow(part, 1)
    still[hole] = 0
    save(still, g["OUT"])

    # The lit still takes the still's alpha, contact shadow and all: the
    # light changes colors, not what's covered.
    glow = lit.copy()
    glow[..., 3] = still[..., 3]
    save(glow, os.path.join(out_dir, "lit.png"))

    def tiles(frames, box):
        return [feather(crop(f, box), box, crop(hole, box), rx, ry) for f in frames]

    layout = {
        "dome": sheet(tiles(dome, dome_box), os.path.join(out_dir, "dome.png")),
        "knob": sheet(tiles(turns, knob_box), os.path.join(out_dir, "knob.png")),
    }
    sheet(tiles(dome_lit, dome_box), os.path.join(out_dir, "dome-lit.png"))

    tube = obs["tube"]
    tube_px = [px(tube.matrix_world @ Vector(co)) for co in g["tube_path"]]
    ball = PLACE @ g["BALL"]
    # The tube's and ball's radii on screen: a point their radius off
    # center, sideways to the camera.
    right = cam.matrix_world.to_3x3() @ Vector((1, 0, 0))
    mid = tube.matrix_world @ Vector(g["tube_path"][len(g["tube_path"]) // 2])
    with open(os.path.join(out_dir, "motion.json"), "w") as f:
        json.dump(
            {
                "width": rx,
                "height": ry,
                "dome": {"box": dome_box, **layout["dome"], "squashes": SQUASHES, "lifts": LIFTS, "lift_px": round(LIFT * scale(px, mid, right), 1)},
                "knob": {"box": knob_box, **layout["knob"], "loop_error": loop_error},
                "tube": {"points": tube_px, "radius": round(0.31 * scale(px, mid, right), 1)},
                "ball": {"center": px(ball), "radius": round(1.15 * scale(px, ball, right), 1)},
            },
            f,
        )


def light_on(g):
    """Switch the tube on: a glowing core in it, the acrylic glowing harder
    where you look along its walls, and the ball it comes from lit up too.

    The core is a sleeve just outside the tube, seen only by diffuse and
    glossy rays: light from inside the clear plastic would reach nothing,
    since Cycles stops shadow rays at glass. The page draws the arc itself."""
    tube = bpy.data.objects["tube"]
    cu = tube.data.copy()
    cu.bevel_depth = 0.36
    core = bpy.data.objects.new("core", cu)
    g["scene"].collection.objects.link(core)
    core.matrix_world = tube.matrix_world
    m = bpy.data.materials.new("arc")
    m.use_nodes = True
    nt = m.node_tree
    nt.nodes.remove(nt.nodes["Principled BSDF"])
    em = nt.nodes.new("ShaderNodeEmission")
    em.inputs["Color"].default_value = (*g["lin"]("#7dff2a"), 1)
    em.inputs["Strength"].default_value = 70.0
    nt.links.new(em.outputs["Emission"], nt.nodes["Material Output"].inputs["Surface"])
    cu.materials.clear()
    cu.materials.append(m)
    core.visible_camera = False
    core.visible_transmission = False
    core.visible_shadow = False
    core.visible_volume_scatter = False
    # Its light falls on everything but the tube itself, whose inside
    # reflections of it would fill the clear plastic in.
    shade = bpy.data.collections.new("not-the-tube")
    shade.objects.link(tube)
    shade.collection_objects[0].light_linking.link_state = "EXCLUDE"
    core.light_linking.receiver_collection = shade

    tube_nodes = g["M"]["tube"].node_tree.nodes
    # Lit, the acrylic glows a deeper neon green.
    tint = next(n for n in tube_nodes if n.bl_idname == "ShaderNodeMix")
    tint.inputs["A"].default_value = (*g["lin"]("#b4ff3c"), 1)
    tint.inputs["B"].default_value = (*g["lin"]("#3fe000"), 1)
    ramp = next(n for n in tube_nodes if n.bl_idname == "ShaderNodeMapRange")
    # Brighter at the edges only: looking straight through, it stays clear,
    # with the page's arc inside it.
    ramp.inputs["To Max"].default_value *= 1.5

    ball = g["M"]["ball"].node_tree.nodes["Principled BSDF"]
    ball.inputs["Emission Color"].default_value = (*g["lin"]("#6cff00"), 1)
    ball.inputs["Emission Strength"].default_value = 0.5


def settle(alpha, cover, fade):
    """radio.py's contact shadow: the radio's coverage, plus the floor's
    shadow only near its footprint."""
    m = cover.astype(np.float32) / 255
    shadow = np.clip(alpha.astype(np.float32) / 255 - m, 0, None)
    return np.round((m + np.minimum(shadow * fade, 0.7 * (1 - m))) * 255).astype(np.uint8)


def load(path, rx, ry):
    image = bpy.data.images.load(path)
    out = np.empty(rx * ry * 4, dtype=np.float32)
    image.pixels.foreach_get(out)
    bpy.data.images.remove(image)
    return np.round(out.reshape(ry, rx, 4)[::-1] * 255).astype(np.uint8)


def save(pixels, path):
    h, w = pixels.shape[:2]
    image = bpy.data.images.new(os.path.basename(path), w, h, alpha=True)
    image.pixels.foreach_set((pixels[::-1].astype(np.float32) / 255).ravel())
    image.filepath_raw = path
    image.file_format = "PNG"
    image.save()
    bpy.data.images.remove(image)


def changed(pairs, seed, block=8, margin=FEATHER + 4):
    """The rectangle (x, y, w, h) round the pixels that differ between each
    (still, frame) pair, joined up with the part at `seed`: its shadows and
    the light it bounces. A change off by itself (the dome in the ball's
    reflection) stays out."""
    ry, rx = pairs[0][0].shape[:2]
    moved = np.zeros((ry, rx), dtype=bool)
    for still, f in pairs:
        moved |= (np.abs(f.astype(int) - still.astype(int)) > SAME).any(axis=2)
    # In blocks, a block counting as changed when enough of it is, then
    # flood-filled from the seed's block across gaps of a block.
    by, bx = -(-ry // block), -(-rx // block)
    padded = np.zeros((by * block, bx * block), dtype=bool)
    padded[:ry, :rx] = moved
    blocks = grow(padded.reshape(by, block, bx, block).mean(axis=(1, 3)) > 0.08, 1)
    # The changed block nearest the seed: a turning knob's middle stays put.
    rr, cc = np.nonzero(blocks)
    k = np.argmin((rr - seed[1] / block) ** 2 + (cc - seed[0] / block) ** 2)
    start = (int(rr[k]), int(cc[k]))
    seen, todo = {start}, [start]
    while todo:
        r, c = todo.pop()
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                n = (r + dr, c + dc)
                if 0 <= n[0] < by and 0 <= n[1] < bx and n not in seen and blocks[n]:
                    seen.add(n)
                    todo.append(n)
    rows, cols = zip(*seen)
    x0, x1 = max(min(cols) * block - margin, 0), min((max(cols) + 1) * block + margin, rx)
    y0, y1 = max(min(rows) * block - margin, 0), min((max(rows) + 1) * block + margin, ry)
    return [x0, y0, x1 - x0, y1 - y0]


def grow(mask, n):
    """Dilate a boolean mask by n pixels (a square)."""
    out = mask.copy()
    for _ in range(n):
        step = out.copy()
        step[1:] |= out[:-1]
        step[:-1] |= out[1:]
        step[:, 1:] |= out[:, :-1]
        step[:, :-1] |= out[:, 1:]
        out = step
    return out


def feather(tile, box, hole, rx, ry):
    """Fade a layer's alpha to nothing over FEATHER pixels at each edge
    that's inside the picture, except over the still's hole, where the
    layer is all there is."""
    x, y, w, h = box

    def ramp(n, at_start, at_end):
        up = np.clip((np.arange(n) + 0.5) / FEATHER, 0, 1) if at_start else np.ones(n)
        down = np.clip((n - np.arange(n) - 0.5) / FEATHER, 0, 1) if at_end else np.ones(n)
        return np.minimum(up, down)

    fade = np.where(hole, 1.0, np.outer(ramp(h, y > 0, y + h < ry), ramp(w, x > 0, x + w < rx)))
    out = tile.copy()
    out[..., 3] = np.round(tile[..., 3] * fade).astype(np.uint8)
    return out


def crop(frame, box):
    x, y, w, h = box
    return frame[y : y + h, x : x + w]


def sheet(tiles, path):
    """Lay the tiles out in a grid, GUTTER apart, and save it."""
    h, w = tiles[0].shape[:2]
    cols = math.ceil(math.sqrt(len(tiles) * h / w))
    rows = math.ceil(len(tiles) / cols)
    pw, ph = w + 2 * GUTTER, h + 2 * GUTTER
    out = np.zeros((rows * ph, cols * pw, 4), dtype=np.uint8)
    for i, t in enumerate(tiles):
        r, c = divmod(i, cols)
        out[r * ph : (r + 1) * ph, c * pw : (c + 1) * pw] = np.pad(t, ((GUTTER, GUTTER), (GUTTER, GUTTER), (0, 0)), mode="edge")
    save(out, path)
    return {"frames": len(tiles), "cols": cols, "rows": rows, "gutter": GUTTER}


def scale(px, p, right):
    """Pixels per cm at p, across the view."""
    a, b = px(p), px(p + right)
    return math.dist(a, b)
