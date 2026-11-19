# Hand tracking models

`gavno-cam` runs two MediaPipe networks on the CPU through tract. The weights
are not stored in git (`/dist/models/*.onnx` is ignored); download them once on
the build PC and `scripts/install-greeter.sh` copies them to
`/usr/local/share/gavno/models/`.

| File | Role | Input | Size | SHA-256 |
|---|---|---|---|---|
| `palm_detection_mediapipe_2023feb.onnx` | palm detector (runs only while no hand is tracked) | 1x192x192x3 NHWC, 0..1 | 3.9 MB | `78ff51c38496b7fc8b8ebdb6cc8c1abb02fa6c38427c6848254cdaba57fcce7c` |
| `handpose_estimation_mediapipe_2023feb.onnx` | 21 hand landmarks (every frame, on the tracked region) | 1x224x224x3 NHWC, 0..1 | 4.1 MB | `db0898ae717b76b075d9bf563af315b29562e11f8df5027a1ef07b02bef6d81c` |

## Source and license

Both files come from the OpenCV Model Zoo on Hugging Face, which converted
Google's MediaPipe TFLite models to ONNX. The license is Apache-2.0 (the texts
are saved next to the weights as `dist/models/LICENSE.*`).

- Palm detector: <https://huggingface.co/opencv/palm_detection_mediapipe>
  (`palm_detection_mediapipe_2023feb.onnx`, repository revision
  `233e619dcea1759bf6de707b9b904fe30881ea55`)
- Landmarks: <https://huggingface.co/opencv/handpose_estimation_mediapipe>
  (`handpose_estimation_mediapipe_2023feb.onnx`, repository revision
  `4b2a0b446e5cf2f11fb6b2c7251091c035d2c1f7`)
- Upstream models: <https://github.com/google/mediapipe/blob/master/docs/solutions/models.md#hands>

Download the files from the `resolve/main/` URL of each repository into
`dist/models/` and compare the SHA-256 sums above:

```sh
mkdir -p dist/models && cd dist/models
curl -LO https://huggingface.co/opencv/palm_detection_mediapipe/resolve/main/palm_detection_mediapipe_2023feb.onnx
curl -LO https://huggingface.co/opencv/handpose_estimation_mediapipe/resolve/main/handpose_estimation_mediapipe_2023feb.onnx
sha256sum -c ../../crates/gavno-cam/MODELS.sha256
```

## How the outputs are used

- Palm detector: 2016 SSD anchors (generated in `vision::palm_anchors`, checked
  against the reference table in a test), box + 7 palm keypoints per anchor,
  logit scores. Only the best palm above 0.55 is used.
- Landmarks: 63 values (21 x, y, z in crop pixels), hand-presence score,
  handedness and world landmarks. A presence score below 0.5 drops the track and
  the palm detector runs again.

The `_int8` variants in the same repositories are smaller but not used.
