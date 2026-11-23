# 차량 중심 정지 어라운드뷰 검증 코드

기존 보정값을 그대로 적용해 **TS 한 프레임 추출 → 카메라 4영역 분리 → 보정 좌표 적용 → 지면 어라운드뷰 합성**을 수행하는 Python 패키지입니다.

이 패키지는 이번 대화에서 검증한 정지 프레임 처리까지 포함합니다. TS 동영상, APK, Android 공유 라이브러리, 신경망 모델은 포함하지 않습니다. 프로그램은 APK나 `.so`를 실행하지 않습니다.

## 1. 설치

Python 3.10 이상, FFmpeg와 ffprobe가 필요합니다. Python 패키지는 다음 명령으로 설치합니다.

```bash
python -m pip install -r requirements.txt
```

Ubuntu / WSL에서 FFmpeg가 없다면 설치합니다.

```bash
sudo apt update
sudo apt install ffmpeg
```

Windows에서는 `ffmpeg.exe`와 `ffprobe.exe`를 PATH에 추가하거나, 아래 실행 옵션으로 경로를 직접 지정합니다. 이 ZIP에는 FFmpeg 실행 파일을 넣지 않았습니다. `pip install ffmpeg`는 시스템 FFmpeg 설치를 대신하지 않습니다.

## 2. 실행

압축을 풀고 `main.py`가 있는 폴더에서 실행합니다.

```bash
python main.py --ts "xxx.ts"
```

기본값은 **영상 시작 기준 5초 지점의 한 프레임**입니다. 출력은 기본적으로 이 패키지의 `output/<TS 파일명>_t5.000s/`에 저장됩니다. 같은 입력명·시점·출력 폴더로 다시 실행하면 해당 결과 파일을 갱신합니다.

처음 프레임을 사용하려면:

```bash
python main.py --ts "xxx.ts" --time 0
```

시점과 출력 폴더를 지정하려면:

```bash
python main.py --ts "xxx.ts" --time 5 --output "result"
```

Windows 경로 예:

```powershell
python main.py --ts "D:\DCIM\Camera\20261113_14h46m56s.ts"
```

WSL 경로 예:

```bash
python3 main.py --ts "/mnt/d/DCIM/Camera/20261113_14h46m56s.ts"
```

Windows의 `D:\...`를 WSL에서는 `/mnt/d/...` 형식으로 바꿔 지정해야 합니다.

FFmpeg 경로를 직접 지정하는 예:

```powershell
python main.py --ts "D:\DCIM\Camera\sample.ts" --ffmpeg "C:\ffmpeg\bin\ffmpeg.exe" --ffprobe "C:\ffmpeg\bin\ffprobe.exe"
```

다른 보정 폴더를 사용하려면 `--config "보정폴더"`를 지정합니다. 해당 폴더에는 `calibration.json`과 `render.json`이 있어야 합니다. 상대 입력·출력 경로는 명령을 실행한 폴더 기준이고, 기본 config와 기본 output은 `main.py`의 위치 기준입니다.

전체 옵션:

```bash
python main.py --help
```

## 3. 출력 파일

| 파일 | 내용 |
| --- | --- |
| `surround_preview.png` | 중앙 차량 영역, 전방 표시, 설명이 포함된 합성 이미지 |
| `surround.png` | 제목·주석 없이 합성한 지면 이미지. 중앙 차량 영역은 가림 처리 |
| `source_frame.png` | TS에서 추출한 원본 프레임 |
| `source_labeled.png` | 전방·후방·좌측·우측을 표시한 비교 이미지 |
| `cameras/cam0_front.png` | 분리한 전방 타일 |
| `cameras/cam1_rear.png` | 분리한 후방 타일 |
| `cameras/cam2_left.png` | 분리한 좌측 타일 |
| `cameras/cam3_right.png` | 분리한 우측 타일 |
| `report.json` | 입력 정보, 요청 시점, 적용한 보정 파일 해시, 렌더 설정과 처리 내역 |

## 4. 코드와 config 구성

| 위치 | 역할 |
| --- | --- |
| `main.py` | CLI, ffprobe/FFmpeg 실행, 프레임 분리와 결과 저장 |
| `surround_view/geometry.py` | 렌즈 투영, 픽셀 보간, 지면 매핑, 카메라 영상 혼합 |
| `surround_view/render.py` | 설명용 이미지와 비교 이미지 생성. 사용할 수 있는 글꼴로 대체하여 Windows/WSL의 특정 글꼴 경로에 의존하지 않음 |
| `surround_view/sphere_lens.py` | 분석한 SphereTable 렌즈 파일 읽기와 투영·역투영 계산 |
| `tools/refit_calibration.py` | 기존 보정점에서 매핑을 다시 계산하는 선택 실행 도구 |
| `config/calibration.json` | **일반 실행에서 실제로 읽는** 렌즈 계수, 영상 중심점, 지면 homography와 차량 크기 |
| `config/render.json` | 출력 해상도, 지면 표시 범위, 혼합 가중치 등 변경 가능한 렌더 값 |
| `config/calibration_points.json` | 앞서 원본 보정 설정과 native 코드를 분석해 얻은 영상·지면 보정점과 필드 |
| `config/360CalibConfig`, `config/360ViewConfig` | 업로드한 원본 보정·뷰 설정 |
| `config/0.brp`–`config/3.brp` | 업로드한 원본 렌즈 파일. 현재 저장 매핑의 활성 프로필과 다름 |
| `config/1002A_2053_1080p.brp` | 설정에 지정된 이름으로 APK에서 추출하여 이번 검증에 사용한 렌즈 프로필 |
| `config/.fourcamera2_param`, `config/.fourcamera_image` | 업로드한 앱 설정 원본. 점으로 시작하는 파일도 ZIP에 포함 |
| `config/calibration_frames/` | 업로드한 카메라별 1920×1080 NV21 보정 프레임 4개 |
| `config/source_manifest.json` | 동봉한 원본 파일의 출처·크기·SHA-256 |

원본 설정과 NV21는 보관 및 검증 근거입니다. **일반 실행은 `calibration.json`의 계산된 값을 읽고 재피팅하지 않습니다.** 원본 `360CalibConfig` 또는 `0.brp`를 수정하는 것만으로 실행 결과가 자동 변경되지는 않습니다. 렌즈 값과 지면 매핑은 서로 대응하므로 다른 카메라의 값으로 일부만 교체해서는 같은 보정으로 볼 수 없습니다.

### 변경 가능한 렌더 값

`config/render.json`에 기존 값을 그대로 분리했습니다.

| 키 | 기본값 | 의미 |
| --- | --- | --- |
| `output_width`, `output_height` | 1000, 1450 | 주석을 제외한 지면 이미지 크기(px) |
| `ground_x_half_extent_m` | 4.0 | 차량 중심에서 좌우 표시 반경. 전체 폭 8m |
| `ground_y_half_extent_m` | 5.8 | 차량 중심에서 앞뒤 표시 반경. 전체 길이 11.6m |
| `blend_cosine_power` | 3 | 카메라가 향한 방향에 따른 혼합 가중치 |
| `blend_edge_fade_px` | 45 | 원본 카메라 좌표계에서 가장자리 가중치가 줄어드는 폭 |
| `minimum_brightness` | 7 | 기존 검증 코드의 어두운 픽셀 제외 기준. 낮은 값도 실제 검은 지면을 일부 제외할 수 있음 |
| `weight_epsilon` | 0.0000001 | 유효 합성 가중치 판정값 |
| `native_border_px` | 1 | 원본 카메라 좌표계의 가장자리 제외 폭 |
| `background_rgb`, `vehicle_fill_rgb` | RGB 배열 | 빈 영역 및 중앙 차량 영역 색 |

차량 영역 크기는 `calibration.json`의 `vehicle.width_m`, `vehicle.length_m`을 사용합니다. 설정에 저장된 값이며 실제 차량을 실측해 검증한 값은 아닙니다.

## 5. 현재 보정 처리

입력은 같은 카메라 설치·설정으로 녹화한 2×2 화면을 전제로 합니다.

| 타일 | 카메라 |
| --- | --- |
| 왼쪽 위 | 0 전방 |
| 오른쪽 위 | 1 후방 |
| 왼쪽 아래 | 2 좌측 |
| 오른쪽 아래 | 3 우측 |

보정 좌표는 카메라별 1920×1080 기준입니다. 전체 TS가 1920×1080이면 각 타일은 960×540이므로, **계산된 영상 좌표에만 0.5배**를 적용합니다. 렌즈 계수까지 이중으로 축소하지 않습니다. 같은 종횡비의 다른 짝수 해상도에서는 타일 크기에 따라 배율을 계산하며, 추가 크롭이나 반전은 자동 추정하지 않습니다.

렌즈 모델은 xi 계수를 사용하는 unified sphere 모델과 radial k1/k2 보정입니다. 일반적인 theta 다항식 OpenCV fisheye 모델로 대체하지 않았습니다. 기본 동봉 값의 p1/p2는 모두 0입니다.

FFmpeg 추출은 영상 시작 기준 시간으로 seek하고 한 프레임을 읽습니다. 이 레코더의 `r_frame_rate`/`avg_frame_rate` 값은 실제 FPS로 쓸 수 없는 경우가 있어 FPS×초 방식으로 선택하지 않습니다. `report.json`의 요청 시각은 정확한 디코딩 PTS나 영상에 찍힌 시계를 판독한 값이 아닙니다.

## 6. 기존 보정 계산을 다시 실행할 때

이 도구는 **동봉한 `calibration_points.json`의 보정점과 동명의 렌즈 프로필**로 이전 계산을 재현합니다. 임의의 새로운 binary 설정 파일이나 다른 보정판 패턴을 자동 분석하는 기능은 아닙니다.

```bash
python tools/refit_calibration.py --output "config/calibration_refit.json"
```

출력 경로는 필수입니다. 기본 `main.py` 실행에서는 이 도구를 호출하지 않습니다. 기존 `calibration.json`은 그대로 둔 채 별도 결과를 비교할 수 있습니다.

## 7. 검증 범위

- 이번 대화의 저장된 렌즈 계수·지면 매핑·혼합 값을 유지했습니다.
- 정지 프레임 한 장의 평면 합성 코드입니다. 연속 영상 재생, 회전 가능한 3D bowl 화면, 차량 3D 모델은 포함하지 않습니다.
- 바닥 위 차량·울타리·사람은 늘어지거나 카메라 경계에서 겹칠 수 있습니다. 가려져 촬영되지 않은 부분을 복원하지 않습니다.
- 기존 보정점에서 계산한 피팅 RMS는 같은 점에 대한 잔차이며, 새 TS의 거리 정확도나 독립 검증 오차를 뜻하지 않습니다.
- 시험 환경과 실제 실행 검증 결과는 `VERIFICATION.json`에 기록합니다.

TS 파일은 ZIP에 포함하지 않았습니다. `--ts`로 사용자 컴퓨터의 원본 파일을 지정하세요.
