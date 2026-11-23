#!/usr/bin/env python3
"""Extract one recorded frame and apply the saved surround-view calibration."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


PACKAGE_DIR = Path(__file__).resolve().parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="TS 한 프레임을 추출하고 기존 보정값으로 정지 어라운드뷰를 합성합니다.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--ts", required=True, type=Path, help="입력 TS 파일 경로")
    parser.add_argument("--time", type=float, default=5.0,
                        help="영상 시작부터 추출할 시점(초). FPS로 프레임 번호를 계산하지 않습니다.")
    parser.add_argument("--output", type=Path,
                        help="결과 폴더. 생략하면 패키지/output/<파일명>_t<초>s")
    parser.add_argument("--config", type=Path, default=PACKAGE_DIR / "config",
                        help="calibration.json 및 render.json이 있는 폴더")
    parser.add_argument("--ffmpeg", default="ffmpeg", help="FFmpeg 명령 또는 실행 파일 경로")
    parser.add_argument("--ffprobe", default="ffprobe", help="ffprobe 명령 또는 실행 파일 경로")
    return parser.parse_args()


def executable(value: str) -> str:
    found = shutil.which(value)
    if found:
        return found
    candidate = Path(value).expanduser()
    if candidate.is_file():
        return str(candidate.resolve())
    raise ValueError(
        f"실행 파일을 찾을 수 없습니다: {value}\n"
        "FFmpeg와 ffprobe를 설치해 PATH에 추가하거나 --ffmpeg/--ffprobe로 경로를 지정하세요."
    )


def run_command(command: list[str], purpose: str) -> str:
    try:
        result = subprocess.run(command, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", timeout=120,
                                shell=False)
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(f"{purpose}: 120초 안에 완료되지 않았습니다.") from exc
    if result.returncode != 0:
        message = result.stderr.strip()[-4000:]
        raise RuntimeError(f"{purpose} 실패 (종료 코드 {result.returncode})\n{message}")
    return result.stdout


def read_object(path: Path) -> dict:
    if not path.is_file():
        raise ValueError(f"설정 파일이 없습니다: {path}")
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"JSON 최상위 값은 객체여야 합니다: {path}")
    return data


def probe_video(binary: str, source: Path) -> dict:
    fields = (
        "format=format_name,duration,start_time,size,bit_rate:"
        "stream=index,codec_name,codec_type,width,height,pix_fmt,"
        "avg_frame_rate,r_frame_rate,time_base,start_time,duration,"
        "color_range,color_space,color_transfer,color_primaries"
    )
    data = json.loads(run_command([
        binary, "-v", "error", "-select_streams", "v:0",
        "-show_entries", fields, "-of", "json", str(source),
    ], "영상 정보 확인"))
    if not data.get("streams"):
        raise ValueError("입력 파일에서 영상 스트림을 찾을 수 없습니다.")
    return data


def extract_frame(binary: str, source: Path, seconds: float, target: Path) -> None:
    # -vsync 0 works with the FFmpeg shipped by Ubuntu 22.04 as well as newer
    # versions. Do not infer a frame index from this recorder's FPS metadata.
    run_command([
        binary, "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
        "-ss", format(seconds, ".9f"), "-threads", "2", "-noautorotate",
        "-i", str(source), "-map", "0:v:0", "-frames:v", "1",
        "-vsync", "0", "-threads", "2", "-update", "1", str(target),
    ], "프레임 추출")
    if not target.is_file() or target.stat().st_size == 0:
        raise ValueError(
            f"{seconds:g}초에서 프레임을 추출하지 못했습니다. "
            "영상 길이를 확인하거나 --time 0 등 다른 시점을 지정하세요."
        )


def validate_calibration(data: dict) -> None:
    if data.get("schema_version") != 1:
        raise ValueError("지원하는 calibration.json schema_version은 1입니다.")
    if data.get("lens_model") != "unified_sphere_radial":
        raise ValueError("지원하는 렌즈 모델은 unified_sphere_radial입니다.")
    cameras = data.get("cameras", [])
    expected = ["FRONT", "REAR", "LEFT", "RIGHT"]
    if len(cameras) != 4:
        raise ValueError("calibration.json에 카메라 4개가 필요합니다.")
    for index, camera in enumerate(cameras):
        if camera.get("index") != index or camera.get("direction") != expected[index]:
            raise ValueError("카메라 배열은 0 FRONT, 1 REAR, 2 LEFT, 3 RIGHT 순서여야 합니다.")
        if camera.get("tile") != [index % 2, index // 2]:
            raise ValueError("현재 코드는 좌상 전방/우상 후방/좌하 좌측/우하 우측 배치를 사용합니다.")


def main() -> int:
    args = parse_args()
    try:
        if not math.isfinite(args.time) or args.time < 0:
            raise ValueError("--time은 0 이상의 유한한 초 값이어야 합니다.")
        source = args.ts.expanduser().resolve()
        if not source.is_file():
            raise ValueError(f"TS 파일이 없습니다: {source}")
        if source.suffix.lower() != ".ts":
            raise ValueError("--ts에는 .ts 파일을 지정하세요.")
        config_dir = args.config.expanduser().resolve()
        calibration_path = config_dir / "calibration.json"
        settings_path = config_dir / "render.json"
        calibration = read_object(calibration_path)
        settings = read_object(settings_path)
        validate_calibration(calibration)
        ffmpeg = executable(args.ffmpeg)
        ffprobe = executable(args.ffprobe)

        try:
            import numpy as np
            from PIL import Image
            from surround_view.geometry import compose_frame
            from surround_view.render import annotate_ground, make_source_sheet
        except ImportError as exc:
            raise RuntimeError(
                "Python 의존성을 불러오지 못했습니다. "
                "python -m pip install -r requirements.txt 를 실행하세요.\n"
                f"세부 정보: {exc}"
            ) from exc

        started = time.perf_counter()
        metadata = probe_video(ffprobe, source)
        stream = metadata["streams"][0]
        output = (args.output.expanduser().resolve() if args.output else
                  PACKAGE_DIR / "output" / f"{source.stem}_t{args.time:.3f}s")
        output.mkdir(parents=True, exist_ok=True)
        print(f"[1/4] 프레임 추출: {source.name}, 시작 기준 {args.time:g}초", flush=True)
        # A fresh temporary extraction path prevents a failed/out-of-range seek
        # from silently reusing a PNG from an earlier run.
        with tempfile.TemporaryDirectory(prefix=".frame_", dir=output) as temporary:
            extracted = Path(temporary) / "frame.png"
            extract_frame(ffmpeg, source, args.time, extracted)
            with Image.open(extracted) as image:
                image = image.convert("RGB")
                frame = np.asarray(image, dtype=np.float32)

        print(f"[2/4] 4개 카메라 영역 확인: {frame.shape[1]} x {frame.shape[0]}", flush=True)
        print("[3/4] 저장된 보정 좌표 적용 및 지면 합성", flush=True)
        ground, details = compose_frame(frame, calibration, settings)
        preview = annotate_ground(ground, calibration, settings, source.name, args.time)
        source_sheet = make_source_sheet(frame, calibration)
        camera_dir = output / "cameras"
        camera_dir.mkdir(exist_ok=True)
        raw_image = Image.fromarray(frame.astype(np.uint8))
        raw_image.save(output / "source_frame.png")
        tile_width, tile_height = raw_image.width // 2, raw_image.height // 2
        camera_files = []
        for camera in calibration["cameras"]:
            col, row = camera["tile"]
            x, y = col * tile_width, row * tile_height
            name = f"cam{camera['index']}_{camera['direction'].lower()}.png"
            raw_image.crop((x, y, x + tile_width, y + tile_height)).save(camera_dir / name)
            camera_files.append(f"cameras/{name}")
        Image.fromarray(ground).save(output / "surround.png")
        preview.save(output / "surround_preview.png")
        source_sheet.save(output / "source_labeled.png")
        artifacts = ["source_frame.png", "source_labeled.png", "surround.png",
                     "surround_preview.png", *camera_files, "report.json"]
        report = {
            "input_file": str(source),
            "input_size_bytes": source.stat().st_size,
            "requested_time_seconds": args.time,
            "timestamp_policy": "Seek by time relative to input start; no FPS-to-frame conversion or forced output FPS.",
            "time_note": "Requested seek time, not a claim about exact decoded PTS or the burned-in camera clock.",
            "video": stream,
            "container": metadata.get("format", {}),
            "decoded_frame_size": [raw_image.width, raw_image.height],
            "tile_size": [tile_width, tile_height],
            "calibration_file": str(calibration_path),
            "calibration_sha256": hashlib.sha256(calibration_path.read_bytes()).hexdigest(),
            "render_settings": settings,
            "calibration_refitted": False,
            "rendering": details,
            "elapsed_seconds": round(time.perf_counter() - started, 3),
            "limits": [
                "Single frame on a flat ground plane; no full-video or interactive 3D renderer.",
                "Elevated objects may stretch, duplicate, or misalign at camera seams.",
                "Assumes the four native camera images are resized into the configured quadrants without extra crop or mirroring.",
                "Same-point calibration fitting residuals are not accuracy measurements for this input TS.",
            ],
            "artifacts": artifacts,
        }
        (output / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print("[4/4] 저장 완료", flush=True)
        print(f"합성 화면: {output / 'surround_preview.png'}")
        print(f"원본 비교: {output / 'source_labeled.png'}")
        print(f"처리 내역: {output / 'report.json'}")
        return 0
    except (ValueError, OSError, RuntimeError, KeyError) as exc:
        print(f"오류: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
