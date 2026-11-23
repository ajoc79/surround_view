"""Apply saved ground-plane calibration to one RGB 2x2 camera frame.

This module performs no fitting, file IO, decoding, or Android library calls.
The projection, interpolation, masks, and blend arithmetic retain the static
prototype's operation order so its 1920x1080 input remains reproducible.
"""
from __future__ import annotations

import numpy as np
from scipy.ndimage import map_coordinates

from .sphere_lens import SphereLens, radial_monotonic_limit


def project(q, lens, return_valid=False):
    """Project camera-local rays with the original vectorized sphere formula."""
    q = np.asarray(q, dtype=np.float64)
    norm = np.linalg.norm(q, axis=-1)
    den = q[..., 2] + lens.xi * norm
    with np.errstate(divide="ignore", invalid="ignore", over="ignore"):
        a = q[..., 0] / den
        b = q[..., 1] / den
        r2 = a * a + b * b
        scale = 1 + lens.k1 * r2 + lens.k2 * r2 * r2
        u = lens.fx * a * scale + lens.cx
        v = lens.fy * b * scale + lens.cy
    uv = np.stack([u, v], axis=-1)
    if return_valid:
        valid = ((den > 1e-9) & np.isfinite(uv).all(-1)
                 & (r2 < radial_monotonic_limit(lens) ** 2))
        if lens.xi > 1:
            valid &= 1 + lens.xi * q[..., 2] / np.maximum(norm, 1e-9) > 0
        return uv, valid
    return uv


def sample(image, uv):
    """Use the prototype's bilinear RGB sampling and zero outside the image."""
    uv = np.nan_to_num(uv, nan=-100, posinf=-100, neginf=-100)
    coords = [uv[..., 1], uv[..., 0]]
    return np.stack([
        map_coordinates(image[:, :, ch], coords, order=1, mode="constant",
                        cval=0, prefilter=False)
        for ch in range(3)
    ], -1)


def frame_layout(frame, calibration):
    """Validate the four equal tiles and return RGB data and image dimensions.

    Returns (frame_float32, native_width, native_height, tile_width, tile_height).
    The input must contain complete camera views scaled without extra cropping.
    """
    frame = np.asarray(frame)
    if frame.ndim != 3 or frame.shape[2] != 3:
        raise ValueError("frame must be an RGB array with shape (height, width, 3)")
    height, width = frame.shape[:2]
    if height < 4 or width < 4 or height % 2 or width % 2:
        raise ValueError("frame width and height must be positive even sizes for a 2x2 layout")
    sizes = calibration["native_camera_size"]
    if len(sizes) != 2 or any(int(v) != v or v <= 0 for v in sizes):
        raise ValueError("native_camera_size must contain positive integer [width, height]")
    native_width, native_height = map(int, sizes)
    tile_width, tile_height = width // 2, height // 2
    if tile_width * native_height != tile_height * native_width:
        raise ValueError(
            f"Input tile aspect ratio {tile_width}x{tile_height} differs from "
            f"native calibration {native_width}x{native_height}; cropping or "
            "non-uniform scaling needs a separately verified coordinate mapping."
        )
    cameras = calibration["cameras"]
    if len(cameras) != 4:
        raise ValueError("calibration must contain exactly four cameras")
    tiles = [tuple(camera["tile"]) for camera in cameras]
    if set(tiles) != {(0, 0), (1, 0), (0, 1), (1, 1)}:
        raise ValueError("camera tile values must cover each 2x2 [column, row] once")
    if len({camera["index"] for camera in cameras}) != 4:
        raise ValueError("camera indices must be unique")
    frame = np.asarray(frame, dtype=np.float32)
    if not np.isfinite(frame).all():
        raise ValueError("frame must contain finite RGB values")
    return frame, native_width, native_height, tile_width, tile_height


def compose_frame(frame: np.ndarray, calibration: dict, settings: dict):
    """Return (uint8 RGB ground pixels, details) without modifying the inputs.

    The supplied ground homographies and lens coefficients are applied as-is.
    Native camera coordinates are scaled only when sampling the recorded tile.
    """
    frame, nw, nh, tw, th = frame_layout(frame, calibration)
    width, height = settings["output_width"], settings["output_height"]
    if any(int(v) != v or v < 2 for v in (width, height)):
        raise ValueError("output_width and output_height must be integers of at least 2")
    width, height = int(width), int(height)
    xlim, ylim = (float(settings["ground_x_half_extent_m"]),
                  float(settings["ground_y_half_extent_m"]))
    power = settings["blend_cosine_power"]
    fade = float(settings["blend_edge_fade_px"])
    border = float(settings["native_border_px"])
    threshold = float(settings["minimum_brightness"])
    epsilon = float(settings["weight_epsilon"])
    if not np.isfinite([xlim, ylim, power, fade, border, threshold, epsilon]).all():
        raise ValueError("render settings must be finite")
    if min(xlim, ylim, fade) <= 0 or power < 0 or border < 0 or epsilon < 0:
        raise ValueError("Ground extents and blend fade must be positive; powers and tolerances nonnegative")
    if 2 * border >= min(nw, nh):
        raise ValueError("native_border_px leaves no usable camera area")
    vehicle_width = float(calibration["vehicle"]["width_m"])
    vehicle_length = float(calibration["vehicle"]["length_m"])
    if not np.isfinite([vehicle_width, vehicle_length]).all() or min(vehicle_width, vehicle_length) <= 0:
        raise ValueError("Vehicle width and length must be finite positive values")
    for key in ("background_rgb", "vehicle_fill_rgb"):
        color = settings[key]
        if len(color) != 3 or any(int(v) != v or not 0 <= v <= 255 for v in color):
            raise ValueError(f"{key} must contain three integer RGB values between 0 and 255")

    xx, yy = np.meshgrid(np.linspace(xlim, -xlim, width),
                         np.linspace(ylim, -ylim, height))
    ground_h = np.stack([xx, yy, np.ones_like(xx)], -1)
    accumulation = np.zeros((height, width, 3))
    total = np.zeros((height, width))
    scale = np.asarray([tw / nw, th / nh])
    camera_details = []

    for camera in calibration["cameras"]:
        lens = SphereLens(**camera["lens_parameters"])
        numeric_lens = [lens.fx, lens.fy, lens.xi, lens.k1, lens.k2,
                        lens.p1, lens.p2, lens.cx, lens.cy]
        if not np.isfinite(numeric_lens).all() or min(lens.fx, lens.fy) <= 0:
            raise ValueError(f"Camera {camera['index']} has invalid lens parameters")
        if lens.p1 != 0 or lens.p2 != 0:
            raise ValueError("This saved radial-only renderer requires p1=p2=0")
        if tuple(camera["principal_center"]) != (lens.cx, lens.cy):
            raise ValueError(f"Camera {camera['index']} principal_center must match lens cx, cy")
        mapping = np.asarray(camera["ground_homography"], dtype=np.float64)
        center = np.asarray(camera["blend_camera_center_metres"], dtype=np.float64)
        normal = np.asarray(camera["outward_normal"], dtype=np.float64)
        if mapping.shape != (3, 3) or not np.isfinite(mapping).all():
            raise ValueError("ground_homography must be a finite 3x3 matrix")
        if center.shape != (3,) or not np.isfinite(center).all():
            raise ValueError("blend_camera_center_metres must contain three finite values")
        if normal.shape != (2,) or not np.isfinite(normal).all() or np.linalg.norm(normal) == 0:
            raise ValueError("outward_normal must be a nonzero finite 2D vector")
        column, row = map(int, camera["tile"])
        ox, oy = column * tw, row * th
        native_uv, valid = project(ground_h @ mapping.T, lens, True)
        tile_uv = native_uv * scale
        valid &= ((native_uv[..., 0] >= border) & (native_uv[..., 0] < nw-border)
                  & (native_uv[..., 1] >= border) & (native_uv[..., 1] < nh-border)
                  & (tile_uv[..., 0] >= 0) & (tile_uv[..., 0] <= tw-1)
                  & (tile_uv[..., 1] >= 0) & (tile_uv[..., 1] <= th-1))
        tile = frame[oy:oy+th, ox:ox+tw]
        color = sample(tile, tile_uv)
        delta = np.stack([xx-center[0], yy-center[1]], -1)
        cosine = np.einsum("...i,i->...", delta, normal) / np.maximum(
            np.linalg.norm(delta, axis=-1), 1e-9)
        edge = np.minimum.reduce([native_uv[..., 0], nw-1-native_uv[..., 0],
                                  native_uv[..., 1], nh-1-native_uv[..., 1]])
        weight = np.maximum(cosine, 0)**power * np.clip(edge/fade, 0, 1)
        weight *= valid * (color.max(-1) > threshold)
        accumulation += color * weight[..., None]
        total += weight
        camera_details.append({
            "index": camera["index"], "direction": camera["direction"],
            "tile_origin_px": [ox, oy], "tile_size_px": [tw, th],
            "native_calibration_size_px": [nw, nh],
            "principal_center_native_px": list(camera["principal_center"]),
            "native_to_tile_scale": scale.tolist(), "mapping_refitted": False,
            "lens_profile": calibration.get("lens_profile"),
        })

    visible = total > epsilon
    pixels = np.full((height, width, 3), settings["background_rgb"], dtype=np.uint8)
    pixels[visible] = np.clip(
        accumulation[visible] / total[visible, None], 0, 255
    ).astype("uint8")
    car_mask = (abs(xx) < vehicle_width/2) & (abs(yy) < vehicle_length/2)
    pixels[car_mask] = settings["vehicle_fill_rgb"]
    outside = ~car_mask
    coverage = float(visible[outside].mean()) if outside.any() else None
    details = {
        "input_size_px": [frame.shape[1], frame.shape[0]],
        "output_size_px": [width, height],
        "native_camera_size_px": [nw, nh], "tile_size_px": [tw, th],
        "native_to_tile_scale": scale.tolist(),
        "calibration_refitted": False,
        "plane_extent_metres": [-xlim, xlim, -ylim, ylim],
        "vehicle_rectangle_metres": [vehicle_width, vehicle_length],
        "coverage_fraction_outside_vehicle": coverage,
        "coverage_note": "Fraction of ground pixels with valid samples, not a calibration accuracy metric.",
        "cameras": camera_details,
    }
    return pixels, details
