"""Portable Pillow annotations for the static ground view and source tiles."""
from __future__ import annotations

import os
from pathlib import Path
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFont

from .geometry import frame_layout


def _font(size):
    """Try a portable font name, system fonts, Windows Arial, then Pillow."""
    candidates = [
        "DejaVuSans.ttf",
        str(Path(sys.prefix) / "share/fonts/truetype/dejavu/DejaVuSans.ttf"),
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/local/share/fonts/DejaVuSans.ttf",
        "/Library/Fonts/DejaVuSans.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        str(Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts/arial.ttf"),
        "arial.ttf",
    ]
    for candidate in candidates:
        try:
            return ImageFont.truetype(candidate, size)
        except (OSError, ValueError):
            continue
    try:
        return ImageFont.load_default(size=size)
    except TypeError:  # Compatibility with an older Pillow installation.
        return ImageFont.load_default()


def annotate_ground(ground_pixels, calibration, settings,
                    source_name: str, time_seconds: float) -> Image.Image:
    """Add the original vehicle outline, forward arrow, labels, and margins."""
    pixels = np.asarray(ground_pixels)
    width, height = int(settings["output_width"]), int(settings["output_height"])
    if pixels.shape != (height, width, 3) or pixels.dtype != np.uint8:
        raise ValueError("ground_pixels must be the uint8 RGB output of compose_frame")
    if not np.isfinite(time_seconds) or time_seconds < 0:
        raise ValueError("time_seconds must be finite and nonnegative")
    xlim = float(settings["ground_x_half_extent_m"])
    ylim = float(settings["ground_y_half_extent_m"])
    vehicle_width = float(calibration["vehicle"]["width_m"])
    vehicle_length = float(calibration["vehicle"]["length_m"])
    ground = Image.fromarray(pixels).copy()
    drawing = ImageDraw.Draw(ground)
    font, title_font = _font(22), _font(29)

    def pt(x, y):
        return ((xlim-x)/(2*xlim)*(width-1), (ylim-y)/(2*ylim)*(height-1))

    drawing.rectangle((*pt(vehicle_width/2, vehicle_length/2),
                       *pt(-vehicle_width/2, -vehicle_length/2)),
                      outline=(147, 177, 203), width=3)
    cx, cy = pt(0, 0)
    labels = [(-25, "VEHICLE AREA"),
              (10, f"{vehicle_width:.2f} m x {vehicle_length:.2f} m")]
    for dy, label in labels:
        bounds = drawing.textbbox((0, 0), label, font=font)
        drawing.text((cx-(bounds[2]-bounds[0])/2, cy+dy), label, font=font,
                     fill=(225, 233, 245))
    arrow_y = pt(0, vehicle_length/2-0.55)[1]
    drawing.line((cx, arrow_y+80, cx, arrow_y), fill=(100, 200, 255), width=6)
    drawing.polygon([(cx, arrow_y-15), (cx-15, arrow_y+14), (cx+15, arrow_y+14)],
                    fill=(100, 200, 255))
    plane_label = f"{2*xlim:.1f} m x {2*ylim:.1f} m ground plane"
    for xy, label in [((18, 18), "FRONT"), ((18, height-45), plane_label)]:
        drawing.text(xy, label, font=font, fill="white",
                     stroke_fill="black", stroke_width=2)

    title_height, footer_height = 92, 67
    canvas = Image.new("RGB", (width, height+title_height+footer_height), (18, 22, 30))
    canvas.paste(ground, (0, title_height))
    drawing = ImageDraw.Draw(canvas)
    source_type = "TS" if Path(source_name).suffix.lower() == ".ts" else "frame"
    drawing.text((22, 14), f"Recorded {source_type}: saved calibration applied",
                 font=title_font, fill="white")
    drawing.text((22, 54), f"{source_name} | frame near +{time_seconds:g} seconds",
                 font=font, fill=(173, 190, 210))
    drawing.text((22, height+104),
                 "Ground-plane preview; elevated objects can stretch or duplicate.",
                 font=font, fill=(173, 190, 210))
    drawing.text((22, height+130),
                 "Existing lens + mapping unchanged; no 3D vehicle model.",
                 font=font, fill=(173, 190, 210))
    return canvas


def make_source_sheet(frame: np.ndarray, calibration: dict) -> Image.Image:
    """Label the recorded four quadrants in place, without any mirroring."""
    frame, _nw, _nh, tw, th = frame_layout(frame, calibration)
    label_height = 40
    sheet = Image.new("RGB", (frame.shape[1], 2*(th+label_height)), (18, 22, 30))
    drawing = ImageDraw.Draw(sheet)
    font = _font(22)
    for camera in calibration["cameras"]:
        column, row = map(int, camera["tile"])
        ox, oy = column*tw, row*th
        target_y = row*(th+label_height)
        tile = np.clip(frame[oy:oy+th, ox:ox+tw], 0, 255).astype("uint8")
        sheet.paste(Image.fromarray(tile), (ox, target_y+label_height))
        label = f"CAM {camera['index']} | {camera['direction']} | recorded {tw} x {th}"
        drawing.text((ox+15, target_y+6), label, font=font, fill="white")
    return sheet
