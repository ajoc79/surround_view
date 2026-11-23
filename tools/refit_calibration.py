"""Optionally reproduce the prototype's fit from its saved calibration points.

The fitted ground mapping and blend centre are reconstructed estimates, not
original camera extrinsics read from the device. Residuals reuse the fit points
and do not measure independent accuracy. An explicit output path is required.
"""
from __future__ import annotations

import argparse
from dataclasses import asdict
import json
from pathlib import Path
import sys

import numpy as np
from scipy.optimize import least_squares
from scipy.spatial.transform import Rotation


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PACKAGE_ROOT))

from surround_view.sphere_lens import (  # noqa: E402
    radial_monotonic_limit,
    read_sphere_brp,
    unproject_sphere_native,
)


def project(q, lens, return_valid=False):
    """Use the same vectorized sphere projection as the original prototype."""
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
        valid = (
            (den > 1e-9)
            & np.isfinite(uv).all(-1)
            & (r2 < radial_monotonic_limit(lens) ** 2)
        )
        if lens.xi > 1:
            valid &= 1 + lens.xi * q[..., 2] / np.maximum(norm, 1e-9) > 0
        return uv, valid
    return uv


def homography_rays(world, rays):
    """Fit a plane-to-ray homography with the original normalized DLT."""
    xy = world[:, :2]
    mean = xy.mean(0)
    scale = np.sqrt(2) / np.sqrt(np.mean(np.sum((xy - mean) ** 2, axis=1)))
    T = np.array([
        [scale, 0, -scale * mean[0]],
        [0, scale, -scale * mean[1]],
        [0, 0, 1.0],
    ])
    w = (T @ np.column_stack([xy, np.ones(len(xy))]).T).T
    rows = []
    for q, r in zip(w, rays):
        S = np.array([
            [0, -r[2], r[1]],
            [r[2], 0, -r[0]],
            [-r[1], r[0], 0],
        ])
        rows.append(np.kron(S, q.reshape(1, 3)))
    _, _, V = np.linalg.svd(np.vstack(rows))
    H = V[-1].reshape(3, 3) @ T
    proj = (H @ np.column_stack([xy, np.ones(len(xy))]).T).T
    if np.sum(proj * rays) < 0:
        H = -H
    return H


def fit_camera(camera_points, lens):
    """Reproduce the prototype's pose fit and separate planar refinement.

    The pose is used only to reproduce its blending centre. The planar
    homography is fitted separately; it is not a recovered native 3D pose.
    """
    world = np.asarray(camera_points["world_mark_points"], dtype=float)
    pixels = np.asarray(camera_points["source_mark_points"], dtype=float)
    if world.shape != (8, 3) or pixels.shape != (8, 2):
        raise ValueError("Each supplied camera must have 8 world and image points")
    if not np.isfinite(world).all() or not np.isfinite(pixels).all():
        raise ValueError("Calibration points must be finite")
    if not np.all(world[:, 2] == 0):
        raise ValueError("This tool supports the supplied Z=0 ground points only")

    rays = np.asarray([unproject_sphere_native(p, lens) for p in pixels])
    # Native ground X points left. Choose the prototype's explicit +Z-up
    # extension and a right-handed frame for its 6DOF fitting step.
    F = np.diag([-1.0, 1.0, 1.0])
    world_rh = world @ F
    H = homography_rays(world_rh, rays)
    H *= 2 / (np.linalg.norm(H[:, 0]) + np.linalg.norm(H[:, 1]))
    approx = np.column_stack([H[:, 0], H[:, 1], np.cross(H[:, 0], H[:, 1])])
    U, _, V = np.linalg.svd(approx)
    R = U @ np.diag([1, 1, np.linalg.det(U @ V)]) @ V
    initial = np.r_[Rotation.from_matrix(R).as_rotvec(), H[:, 2]]

    def residual(x):
        rotation = Rotation.from_rotvec(x[:3]).as_matrix()
        return (project(world_rh @ rotation.T + x[3:], lens) - pixels).ravel()

    result = least_squares(
        residual, initial, method="lm", max_nfev=2000,
        ftol=1e-12, xtol=1e-12, gtol=1e-12,
    )
    R = Rotation.from_rotvec(result.x[:3]).as_matrix()
    t = result.x[3:]
    M = R @ F
    Hnative = H @ F
    wh = np.column_stack([world[:, :2], np.ones(len(world))])

    anchor = int(np.argmax(np.abs(Hnative.ravel())))
    Hnorm = Hnative / abs(Hnative.ravel()[anchor])
    anchor_value = float(Hnorm.ravel()[anchor])
    free = np.arange(9) != anchor

    def unpack_h(x):
        vals = np.ones(9)
        vals[anchor] = anchor_value
        vals[free] = x
        return vals.reshape(3, 3)

    def residual_h(x):
        return (project(wh @ unpack_h(x).T, lens) - pixels).ravel()

    result_h = least_squares(
        residual_h, Hnorm.ravel()[free], method="lm", max_nfev=2000,
        ftol=1e-12, xtol=1e-12, gtol=1e-12,
    )
    Hfit = unpack_h(result_h.x)
    if np.sum((wh @ Hfit.T) * rays) < 0:
        Hfit = -Hfit
    errors = np.linalg.norm(project(wh @ Hfit.T, lens) - pixels, axis=1)
    if not result.success or not result_h.success:
        raise ValueError(
            f"Camera {camera_points['index']} fit did not converge: "
            f"pose={result.message}; ground={result_h.message}"
        )
    centre = -M.T @ t
    if not (np.isfinite(Hfit).all() and np.isfinite(centre).all()
            and np.isfinite(errors).all()):
        raise ValueError(f"Camera {camera_points['index']} fit produced nonfinite values")
    return {
        "lens_parameters": asdict(lens),
        "ground_homography": Hfit.tolist(),
        "blend_camera_center_metres": centre.tolist(),
        "fitting_residual_native_px": {
            "rms": float(np.sqrt(np.mean(errors ** 2))),
            "max": float(errors.max()),
        },
    }


def refit(config_directory):
    """Return a copy of calibration.json with only the four fit fields updated."""
    config_directory = Path(config_directory)
    template = json.loads(
        (config_directory / "calibration.json").read_text(encoding="utf-8")
    )
    points = json.loads(
        (config_directory / "calibration_points.json").read_text(encoding="utf-8")
    )
    point_cameras = {int(c["index"]): c for c in points["cameras"]}
    indices = [int(c["index"]) for c in template["cameras"]]
    if sorted(indices) != [0, 1, 2, 3] or sorted(point_cameras) != [0, 1, 2, 3]:
        raise ValueError("Expected cameras 0, 1, 2, 3 in the template and saved points")
    for camera in template["cameras"]:
        c = point_cameras[int(camera["index"])]
        profile = config_directory / Path(c["fisheye_path"]).name
        lens = read_sphere_brp(profile, center=c["principal_center"])
        camera.update(fit_camera(c, lens))
    return template


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", type=Path, default=PACKAGE_ROOT / "config", metavar="DIR",
        help="Directory containing calibration.json, calibration_points.json and BRP",
    )
    parser.add_argument(
        "--output", type=Path, required=True, metavar="JSON",
        help="Explicit output JSON path; a relative path is relative to the current directory",
    )
    args = parser.parse_args(argv)
    try:
        report = refit(args.config.expanduser())
        output = args.output.expanduser()
        serialized = json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(serialized + "\n", encoding="utf-8")
    except (OSError, ValueError, KeyError, TypeError, np.linalg.LinAlgError) as exc:
        parser.error(str(exc))
    print(f"Saved: {output.resolve()}")
    for camera in report["cameras"]:
        residual = camera["fitting_residual_native_px"]
        print(
            f"Camera {camera['index']}: same-point native-pixel residual "
            f"RMS={residual['rms']:.6f}, max={residual['max']:.6f}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
