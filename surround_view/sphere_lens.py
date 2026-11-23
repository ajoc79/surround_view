"""Static-analysis reconstruction of ivicar::camera::FisheyeSphere.

Evidence: uploaded libavm3dalgo-noui.so AArch64 functions:
  LoadFromFile 0xbc6870, SaveToFile 0xbc7040,
  ProjectPoint 0xbc6744, ReProjectPoint 0xbc65d0,
  SetCenter 0xbc716c, constructor 0xbc7334.
No uploaded executable code is loaded or executed by this module.

Coordinates are camera-local +Z optical axis, X/Y as passed by caller.
The image center comes from 360CalibConfig/SetCenter, NOT from SphereTable BRP.
Functions do not establish vehicle-to-camera extrinsics or a fisheye valid mask.
"""
from dataclasses import dataclass
from pathlib import Path
import math
import struct


@dataclass(frozen=True)
class SphereLens:
    fx: float
    fy: float
    xi: float
    k1: float
    k2: float
    p1: float
    p2: float
    cx: float
    cy: float
    version: int = 0


def read_sphere_brp(path, center):
    """Read the 72-byte SphereTable format; center is an explicit (cx, cy)."""
    data = Path(path).read_bytes()
    if len(data) != 72 or data[:12] != b'SphereTable\x00':
        raise ValueError('Expected 72-byte SphereTable BRP')
    version, *p = struct.unpack_from('<I7d', data, 12)
    return SphereLens(*p, float(center[0]), float(center[1]), version)


def project_sphere(ray, lens, tangential=False):
    """Camera-local 3D ray -> (u,v), matching native ProjectPoint algebra.

    Native starts with float inputs, computes in double, returns float pixels;
    this function retains Python double precision throughout. This function
    does not reject rays outside an image or outside a monotonic lens branch.
    The native constructor initializes its tangential flag to false. BRP does
    not store that flag. Supplied 0..3.brp all have p1=p2=0 either way.
    """
    x, y, z = map(float, ray)
    length = math.sqrt(x*x + y*y + z*z)
    if length == 0:
        raise ValueError('Cannot project zero-length ray')
    x, y, z = x/length, y/length, z/length
    den = z + lens.xi
    if den == 0:
        raise ValueError('Singular sphere projection: normalized Z + xi == 0')
    x, y = x/den, y/den
    r2 = x*x + y*y
    radial = 1.0 + lens.k1*r2 + lens.k2*r2*r2
    xd, yd = x*radial, y*radial
    if tangential:
        xd += 2.0*lens.p1*x*y + lens.p2*(r2 + 2.0*x*x)
        yd += 2.0*lens.p2*x*y + lens.p1*(r2 + 2.0*y*y)
    return lens.fx*xd + lens.cx, lens.fy*yd + lens.cy


def unproject_sphere_native(pixel, lens, tangential=False):
    """Replicate native ReProjectPoint's 20 iterations and sqrt(abs(D)).

    This is intentionally the native formula, including use of abs on a
    negative discriminant. Such output can be nonphysical outside the valid
    domain, and a converged/valid result is not guaranteed by this function.
    Compare project_sphere(result) against the input pixel before using a ray.
    """
    u, v = map(float, pixel)
    xd = (u - lens.cx)/lens.fx
    yd = (v - lens.cy)/lens.fy
    x, y = xd, yd
    for _ in range(20):
        r2 = x*x + y*y
        radial = 1.0 + lens.k1*r2 + lens.k2*r2*r2
        if radial == 0:
            raise ValueError('Singular native radial inversion')
        if tangential:
            # Native updates x before computing the 2*p2*x*y term for y.
            old_x, old_y = x, y
            x = (xd - 2.0*lens.p1*old_x*old_y
                 - lens.p2*(r2 + 2.0*old_x*old_x))/radial
            y = (yd - 2.0*lens.p2*x*old_y
                 - lens.p1*(r2 + 2.0*old_y*old_y))/radial
        else:
            x, y = xd/radial, yd/radial
    r2 = x*x + y*y
    a = 1.0 + r2
    b = 2.0*r2*lens.xi
    c = r2*lens.xi*lens.xi - 1.0
    discriminant = b*b - 4.0*a*c
    z = (math.sqrt(abs(discriminant)) - b)/(2.0*a)
    scale = z + lens.xi
    return x*scale, y*scale, z


def radial_monotonic_limit(lens):
    """First positive r where d[r(1+k1*r²+k2*r⁴)]/dr = 0, or infinity.

    This mathematical branch limit is NOT a manufacturer specified FOV.
    Applies to radial-only lens; requires separate actual image masking.
    """
    a, b = 5.0*lens.k2, 3.0*lens.k1
    if a == 0:
        return math.sqrt(-1.0/b) if b < 0 else math.inf
    disc = b*b - 4.0*a
    if disc < 0:
        return math.inf
    roots = [s for s in ((-b+math.sqrt(disc))/(2*a),
                        (-b-math.sqrt(disc))/(2*a)) if s > 0]
    return math.sqrt(min(roots)) if roots else math.inf
