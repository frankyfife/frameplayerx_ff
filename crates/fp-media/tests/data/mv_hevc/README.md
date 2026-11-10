# MV-HEVC test stream

A two-view MV-HEVC clip for the `mv_hevc` module's tests.

| File | What it covers | Source |
|---|---|---|
| `multiview.mov` | 11 frames of two 1920x1080 views at 30 fps, muxed like Apple's spatial video: `hvcC` with an `lhvC` box, `vexu` (rectilinear projection, stereo views) and a three-dimensional reference displays SEI naming view 0 the right eye and view 1 (almost black) the left | FFmpeg's FATE test suite, `hevc/multiview.mov` (https://fate-suite.ffmpeg.org/hevc/multiview.mov), the stream of FFmpeg's `fate-hevc-mv-position` test |

The conformance streams `MVHEVCS_A.bit`, `MVHEVCS_B.bit` and
`mv_nuh_layer_id.bit` of FFmpeg's FATE suite (`hevc-conformance/`, at
https://fate-suite.ffmpeg.org/) aren't kept here; with them in a folder,
`FPX_MV_HEVC_SAMPLES=<folder> cargo test -p fp-media --lib conformance_streams -- --ignored`
checks that the rewrite for the hardware decoder gives their pictures exactly.
`MVHEVCS_B.bit` predicts motion from the other view, which the rewrite
can't express: there the check is that it gives up and goes on in software
without losing or changing a picture.
