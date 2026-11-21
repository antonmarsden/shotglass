# Design: live mode

Issue: [#51](https://github.com/antonmarsden/shotglass/issues/51). Status: **draft for
review** (open questions at the end). Part of the roadmap #35, "Explore". Builds on the
iChannel design ([ichannel.md](ichannel.md), #22) and on loop mode (#49); shares a
streaming-texture path with video in #32.

## Summary

`shotglass live <film>` runs a film in real time with a **camera** and a **microphone**
as inputs and sends the picture somewhere a video-call app (Zoom, Teams, Meet, FaceTime)
can pick it up. A camera is one more kind of iChannel (a texture that changes every
frame); the microphone is a Shadertoy-style 512×2 texture (spectrum and waveform). Both
are captured on worker threads and uploaded without blocking the render thread, through
the texture path #22 built, plus a small ring of pixel buffers.

Live mode is an **explicit separate mode**. Frames there depend on the camera and the
wall clock, so they are no longer deterministic, and that is fine because nothing offline
sees them: `still`, `strip`, `export`, `wav`, `check` and the golden tests render a live
channel from a **stand-in** (an image, or black), so they stay exactly as deterministic
as they are now.

For the output there is no single crate that makes a virtual camera on every OS (see
[Research](#research-crates-and-platforms)). The recommendation is to get there in
steps, each one useful on its own:

1. **Camera + mic in, window out.** A borderless/full-screen output window. Meeting apps
   can share that window today, and OBS can capture it into **OBS's virtual camera**,
   which exists on Windows, macOS and Linux. Zero new output dependencies. Audio out goes
   to any device `cpal` can open, so a virtual mic driver (VB-Cable, BlackHole, a
   PipeWire null sink) works with only a device picker.
2. **Share the frame with OBS without a window:** Spout on Windows and Syphon on macOS
   (GPU texture sharing, free, permissive licences), and v4l2loopback on Linux (which *is*
   a virtual camera). NDI is the cross-platform alternative but needs a proprietary
   runtime; optional, later.
3. **Native virtual cameras** (Windows 11 Media Foundation, a macOS Camera Extension),
   only if live mode is ever meant for people who won't install OBS. That's where the
   cost is: a COM DLL on Windows; an app bundle, a signed and notarised system extension
   and a paid Apple developer account on macOS.
4. *(Explore)* **Live sound:** the mic as a bus that sound shaders process with low
   latency, and recorded "takes" that re-render deterministically.

Follow-up issues: phase 1 #53, phase 2 #54, phase 3 #55, phase 4 #56.

The macOS built-in camera goes through AVFoundation, via `nokhwa`'s native backend,
which is the recommended capture crate on all three OSes.

## Goals and non-goals

**Goals:** a camera and a mic as shader inputs, on Windows (the owner's RTX 3060), macOS
(built-in FaceTime HD / Continuity camera) and Linux, with one code path where possible;
an output a meeting app can use; no regressions in the invariants (playback never
blocks, no per-frame GPU→CPU syncs, deterministic offline frames).

**Not in this design:** video files as inputs (#32; it will reuse the streaming-texture
path below), the web player's `getUserMedia` (#2), screen capture as an input, face
tracking or segmentation (could come later as a CPU/GPU pass producing a mask channel),
networked multi-machine setups.

## What exists today

- **Textures (#22):** `channels = [...]` binds up to four images; `texture.rs` decodes on
  a worker, `gfx.rs` uploads in bands within `upload_budget` per `pump`, restores unpack
  state, gates shots in `usable` until every bound texture is `Ready`, and counts
  `blocking_texture_loads` (0 while playing, checked by the selftest). Sampler objects
  carry filter/wrap. `iChannelTime[4]` is declared and always 0.
- **Sound:** `sound.rs` synthesises music / per-shot / mix tracks on the GPU in chunks of
  `CHUNK` = 32768 samples (≈0.68 s) ahead of the playhead; `audio.rs` plays them through
  `cpal` on the **default output device** (`AudioOut::open`), with a lock-free `fill`
  callback and the master `Clock`.
- **Readback:** `read_output`/`read_output_into` are synchronous (`export` is headless and
  may block); a PBO readback is an open follow-up of #1. Sound already has the async
  pattern: PBO + fence, polled oldest-first in batches.
- **Player (`app.rs`):** an egui window with the frame in a central panel and the
  timeline/panels around it; no full-screen output view.
- **Per-machine settings:** `movie.local.toml` (`library::LOCAL_FILE`) already holds
  library overrides and isn't bundled, which is the right home for device names.
- **Loop mode (#49, in progress):** `iTime` that wraps within a period so hours of
  playback don't lose float precision. Live mode runs for hours too and relies on it.

## Research: crates and platforms

Checked on **2026-11-21** on crates.io (versions, dates, licences) and by reading the
published crate sources; GitHub and docs.rs weren't reachable from the research session,
so maintenance is judged from release history. "Recent" = downloads in the last 90 days.

### Camera capture

| crate | version (date) | licence | platforms | notes |
|---|---|---|---|---|
| [`nokhwa`](https://crates.io/crates/nokhwa) | 0.10.11 (2026-05-15) | Apache-2.0 | Linux V4L2, Windows MSMF, macOS AVFoundation, web | ~300k recent. Releases 0.10.7 (2024-12) → 0.10.11 (2026-05). Recommended. |
| [`nokhwa-bindings-macos`](https://crates.io/crates/nokhwa-bindings-macos) | 0.2.4 (2026-05-15) | Apache-2.0 | macOS | AVFoundation via the older `objc` 0.2 / `block` / `core-media-sys` crates, not `objc2`. Has the camera-permission request (`nokhwa_initialize`). |
| [`nokhwa-bindings-windows`](https://crates.io/crates/nokhwa-bindings-windows) | 0.4.6 (2026-05-15) | Apache-2.0 | Windows | Media Foundation via `windows` 0.62 (same as current `windows`). |
| [`nokhwa-bindings-linux`](https://crates.io/crates/nokhwa-bindings-linux) | 0.1.4 (2026-05-15) | Apache-2.0 | Linux | via [`v4l`](https://crates.io/crates/v4l) 0.14.0 (2023-05, MIT) and `v4l2-sys-mit` (pregenerated, no bindgen). |
| [`objc2-av-foundation`](https://crates.io/crates/objc2-av-foundation) | 0.3.2 (2025-10-04) | Zlib/Apache-2.0/MIT | macOS | Fallback if nokhwa's macOS backend fails us: our own `AVCaptureSession` + `AVCaptureVideoDataOutput` (a few hundred lines). |
| [`ccap-rs`](https://crates.io/crates/ccap-rs) | 1.6.0 (2026-03-21) | MIT | Win/macOS/Linux | Bindings to a C++ library: a C++ build in every build. |
| [`oximedia-capture`](https://crates.io/crates/oximedia-capture), [`kessel-video`](https://crates.io/crates/kessel-video) | 0.2.1 (2026-08), 0.1.8 (2026-11) | Apache-2.0; MIT/Apache-2.0 | all three | Brand new, part of larger frameworks. Watch, don't depend. |
| [`eye`](https://crates.io/crates/eye), [`camera_capture`](https://crates.io/crates/camera_capture), [`kamera`](https://crates.io/crates/kamera) | 2023, 2017, 2023 | MIT | partial | Unmaintained. |
| [`crabcamera`](https://crates.io/crates/crabcamera) | 0.9.2 (2026-07) | MIT/Apache-2.0 | all three | Tauri-oriented. |
| `opencv` (nokhwa `input-opencv`) | | | all three | A full OpenCV install. No. |

**nokhwa features:** use `default-features = false, features = ["input-native"]`. The
default `decoding` feature pulls in `mozjpeg` (a C library); shotglass already has the
`image` crate with JPEG (`zune-jpeg`, pure Rust), which is enough for MJPEG cameras.
`output-wgpu` is irrelevant (we're OpenGL); `output-threaded` isn't needed (we run our
own capture thread).

**Spike (throwaway, not committed):** a 15-line program with `nokhwa` 0.10.11
(`input-native`, no default features) and `cpal` 0.18.2:
- Linux x86_64: builds with no new system libraries (ALSA headers were already needed
  for `cpal`), no bindgen, no C compiler step; `nokhwa::query` runs and returns 0 cameras
  in a container without `/dev/video*`; `cpal` lists the default input device.
- `cargo check --target x86_64-pc-windows-msvc`: passes.
- `--target aarch64-apple-darwin`: can't be checked from Linux (the macOS backend's
  `objc_exception` dependency compiles an Objective-C file, which needs the macOS SDK).
  The first Phase 1 task is therefore a spike on the owner's Mac.

**macOS permissions.** The camera and the mic are TCC-protected. A CLI run from a
terminal inherits the terminal's permission (the first run shows "Terminal would like to
access the camera"). A bundled `.app` must have `NSCameraUsageDescription` and
`NSMicrophoneUsageDescription` in its `Info.plist` or macOS kills it on first access; a
hardened-runtime (notarised) build also needs the `com.apple.security.device.camera` and
`com.apple.security.device.audio-input` entitlements. nokhwa asks for camera access with
`nokhwa_initialize(callback)`, which must run before opening the camera; the result comes
back asynchronously, so `live` waits for it with a message ("allow camera access in
System Settings → Privacy & Security → Camera") rather than failing. Windows has a
similar privacy switch (Settings → Privacy → Camera, "let desktop apps access your
camera") that shows up as an open error; Linux needs the user in the `video` group.

### Microphone and audio devices

| crate | version (date) | licence | notes |
|---|---|---|---|
| [`cpal`](https://crates.io/crates/cpal) | 0.18.2 (2026-08-16) | Apache-2.0 | Already a dependency. Input streams on every host (WASAPI, CoreAudio, ALSA/PipeWire/PulseAudio/JACK); devices by stable ID or name; `build_input_stream`. CoreAudio needs macOS 14.2+ per its README. |
| [`realfft`](https://crates.io/crates/realfft) | 3.5.1 (2026-10-26) | MIT | Real-input FFT on top of [`rustfft`](https://crates.io/crates/rustfft) 6.4.1 (MIT/Apache-2.0). A 2048-point FFT is microseconds. |
| [`ringbuf`](https://crates.io/crates/ringbuf) | 0.6.0 (2026-10-30) | MIT/Apache-2.0 | SPSC lock-free ring from the input callback to the analysis thread. |

**Virtual microphones** need a driver on every OS; none is ours to ship:
BlackHole (macOS, GPL-3.0, installed by the user), VB-Cable (Windows, donationware),
a PipeWire/PulseAudio null sink on Linux (`pactl load-module module-null-sink
sink_name=shotglass`, whose monitor appears as a source). Each shows up to `cpal` as an
ordinary output device. shotglass only needs to **open a named output device** instead of
the default, which is a small change to `AudioOut::open`. Their licences don't touch
shotglass because they're separate installs.

### Outputs: virtual cameras

| route | crate / API | licence | cost |
|---|---|---|---|
| Windows 11 Media Foundation virtual camera | `MFCreateVirtualCamera` in [`windows`](https://crates.io/crates/windows) 0.62.2 (feature `Win32_Media_MediaFoundation`; `MFVirtualCameraLifetime_Session/System`, `MFVirtualCameraAccess_CurrentUser/AllUsers`, all present in the crate) | MIT/Apache-2.0 | Windows 11 only. The camera is a COM **media source DLL** that the Frame Server service loads in *its* process, so we'd write and register a DLL implementing `IMFMediaSource`/`IMFMediaStream` and feed it frames across processes (shared memory or a shared D3D11 texture). The reference sample is smourier/VCamSample (C++). Large. |
| macOS CoreMediaIO Camera Extension | [`objc2-core-media-io`](https://crates.io/crates/objc2-core-media-io) 0.3.2 has `CMIOExtensionProvider/Device/Stream` and `CMIOExtensionStreamDirectionSink`; [`objc2-system-extensions`](https://crates.io/crates/objc2-system-extensions) 0.3.2 has `OSSystemExtensionRequest` | Zlib/Apache-2.0/MIT | macOS 12.3+. The extension is a separate executable inside an **app bundle** (`Contents/Library/SystemExtensions`), installed with `OSSystemExtensionRequest` from an app in `/Applications`, approved by the user in System Settings. Needs the `com.apple.developer.system-extension.install` entitlement, an app group, **Developer ID signing and notarisation** (Apple Developer Program, $99/year). The app feeds the extension through a sink stream. Large, plus release engineering. |
| Linux v4l2loopback | [`v4l`](https://crates.io/crates/v4l) 0.14.0 (MIT) to open `/dev/videoN` as an output and write frames; [`v4l2loopback`](https://crates.io/crates/v4l2loopback) 0.1.0 (2023, needs an unreleased module, root) only manages devices, not needed | MIT | Small for us. The user installs the out-of-tree kernel module (DKMS; signed for Secure Boot) and runs `modprobe v4l2loopback video_nr=10 exclusive_caps=1` (browsers want `exclusive_caps`). |
| Linux PipeWire camera node | [`pipewire-vircam`](https://crates.io/crates/pipewire-vircam) 0.7.1 (2026-08, Apache-2.0) on [`pipewire`](https://crates.io/crates/pipewire) 0.10.1 | Apache-2.0 | No kernel module, but only apps that read cameras through PipeWire see it (browsers behind a flag/portal today). Young crate. Later. |
| "one API" wrapper | [`virtualcam`](https://crates.io/crates/virtualcam) 1.0.0 (2026-07-22) | **AGPL-3.0** (0.1.x were Apache-2.0) | Windows MF (bundled VCamSample-derived DLL, built and registered with an admin `install.bat`), the OBS Virtual Camera queue, v4l2loopback. No macOS. AGPL would make the shotglass binary AGPL: **not usable**, but its approach is a useful map. |

**Writing straight into OBS's virtual camera.** On Windows, OBS's virtual camera is a
DirectShow filter reading a shared-memory queue, and `virtualcam`'s `obs` backend writes
that queue without OBS running. It gives a virtual camera with only OBS *installed*, but
the queue format is OBS's internal detail. Noted, not recommended.

### Outputs: bridges into OBS (or other apps)

OBS Studio's own **virtual camera** exists on Windows (DirectShow), macOS (a Camera
Extension since OBS 30) and Linux (v4l2loopback). Getting frames into OBS is therefore
enough on every OS, and OBS adds scenes, overlays and audio routing for free.

| bridge | crate | licence | platforms | notes |
|---|---|---|---|---|
| **OBS window capture** of our output window | none | | all | Works with Phase 1 and no code. Costs a compositor copy and some latency; the window must stay unoccluded on some OSes. |
| **Spout** | [`spout2-rs`](https://crates.io/crates/spout2-rs) 0.1.1 (2026-06-19, vendored Spout 2.007.017, D3D11/D3D12/**OpenGL** backends, MSVC x86_64 only); [`spout-rs`](https://crates.io/crates/spout-rs) 0.2.0 (2026-08-24, D3D11 + OpenGL, vendored); [`rust-spout2`](https://crates.io/crates/rust-spout2) 0.1.5 (D3D11 only, MPL-2.0) | BSD-2-Clause | Windows | Zero-copy GPU sharing of a GL texture (WGL_NV_DX_interop, fine on NVIDIA). OBS reads it with the Spout2 plugin for OBS (a separate install). All three crates are months old: pick one, pin it, be ready to vendor. |
| **Syphon** | [`syphon-rs`](https://crates.io/crates/syphon-rs) 0.2.0 (2026-08-24, Metal server with a CPU-upload path, optional vendored framework build needing full Xcode); [`syphon-core`](https://crates.io/crates/syphon-core) 0.4.0 (2026-08-22) + `syphon-metal` | MIT (+BSD-3 for the framework) | macOS | Neither crate wraps `SyphonOpenGLServer`; from our GL context the choices are the CPU path (needs our async readback) or calling `SyphonOpenGLServer` directly with `objc2` (zero copy). OBS on macOS reads Syphon natively. |
| **NDI** | [`grafton-ndi`](https://crates.io/crates/grafton-ndi) 3.0.0 (2026-11-19; releases monthly since 1.0 in 2026-06; ~33k recent) | wrapper Apache-2.0 AND MIT; the **NDI 6 runtime is proprietary** and installed separately | Windows, macOS, Linux (+mobile) | One code path everywhere, also across machines. Loads the runtime at run time (no SDK at build time). Compressed over the loopback network, so more CPU and a frame or two more latency. OBS reads it with the DistroAV plugin. Others (`ndi`, `ndi-sdk`, `ndi-sdk-sys` GPL-3.0) are stale or copyleft. |

### Conclusion

- **Capture:** `nokhwa` (native backends) for cameras, `cpal` for the mic, `realfft` for
  the spectrum. All permissive, all already proven in the spike on Linux/Windows; the Mac
  needs a hands-on spike.
- **Output:** a window first; then Spout (Windows) + Syphon (macOS) + v4l2loopback
  (Linux) as the "no window" bridges; NDI as an optional cargo feature if the
  proprietary runtime is acceptable; native virtual cameras only if a non-OBS audience
  appears.
- **Mic out:** a device picker; the user installs the virtual cable.

## Inputs

### `movie.toml`

Live inputs are new **channel kinds** in the `[textures]` table #22 reserved room for
(inline tables with a different key, see "Later" in ichannel.md):

```toml
[textures]
cam = { camera = "default", size = "1280x720", fps = 30, stand_in = "textures/face.jpg" }
mic = { mic = "default" }        # 512×2 R8: row 0 spectrum, row 1 waveform (Shadertoy)

[[shot]]
id = "booth"
scene = "scenes/booth.glsl"
duration = 60.0
channels = ["cam", "mic", "<kit/textures/noise-rgba-256.png>"]
```

- `camera = "default"` is the only value a film should normally use: **which** camera is
  a per-machine choice, made with `shotglass live --camera "FaceTime"` or in
  `movie.local.toml` (`[live] camera = "FaceTime HD Camera"`, `mic = "…"`, `audio_out =
  "BlackHole 2ch"`), matched by case-insensitive substring of the device name, or an index
  from `shotglass devices`. A film that names a device still loads elsewhere (it falls
  back to the default with a warning).
- `size`/`fps` are *requests*; the camera picks the nearest format it supports, and
  `iChannelResolution[n]` reports what it delivers. Default: 1280x720 at 30, which every
  built-in camera offers.
- `stand_in`: what offline commands (and live mode before the first frame) bind instead:
  an image file (same rules and options as #22), default black. See
  [Determinism](#determinism).
- `mirror` (default `false`): flip horizontally during conversion (on the capture thread
  in Phase 1, in the GPU conversion pass later). Meeting apps mirror their own preview but send the
  unmirrored picture, so `false` keeps text readable for the far end.
- The usual `filter`, `wrap`, `srgb` apply; `filter` defaults to `"linear"` for live
  channels (a mipmap rebuild per camera frame is avoidable cost; `"mipmap"` stays
  available).
- Validation follows the existing rules in `Movie::parse` (one of `file`/`camera`/`mic`
  per entry, known keys only via `deny_unknown_fields`, `size` parses, `fps` in 1..=120,
  at most one `camera` device per film in Phase 1).

`iChannelTime[n]` becomes meaningful for live channels: the capture timestamp of the
frame on screen, in the same clock as `iTime`.

### Camera pipeline

```text
camera ──► capture thread (nokhwa) ──► convert to RGBA8 ──► latest-frame slot (ArcSwap)
                                                               │  newest only, older dropped
render thread: pump ──► PBO ring (2–3) ──► glTexSubImage2D ──► live texture ──► draw_scene
```

- **One capture thread per camera** owns the `nokhwa::Camera` and calls `frame()` in a
  loop (it blocks until the next frame, which is why it's a thread). It converts to RGBA8
  (YUYV/NV12 → RGB with the BT.601/709 matrix the format implies; MJPEG through `image`'s
  JPEG decoder), applies nothing else, and publishes the frame with its timestamp into an
  `ArcSwap<Option<Arc<CameraFrame>>>`. Older frames are simply replaced: live mode always
  shows the newest frame, never a queue. A 720p RGBA frame is 3.7 MB; conversion at 30 fps
  is a few ms per frame of one core.
- **GPU-side conversion later.** Uploading NV12 as two planes (R8 + RG8) and converting in
  a tiny pass halves upload bandwidth and frees the CPU; worth it at 1080p60. Phase 1
  converts on the CPU, which is simpler and the same on every platform.
- **Uploads use a pixel-unpack buffer ring**, not client memory: copy the frame into the
  next PBO (persistently mapped on GL 4.4, like the sound path; `glBufferSubData` with
  orphaning otherwise), then `tex_sub_image_2d` from the PBO offset. The copy into the PBO
  is a memcpy on our side; the transfer to the texture is asynchronous in the driver. A
  PBO is reused only once its previous upload's fence has passed (polled oldest-first
  with `glClientWaitSync(.., 0)` in the existing batches); if none is free, this frame's
  camera image is skipped and the previous one stays. No waiting anywhere.
- **Budget:** one camera upload per film frame at most, outside the GPU timer, counted
  as housekeeping (a stall, never a drop, so it doesn't lower the auto scale), shown as
  "camera" in the Performance panel next to "textures".
- **Gating:** a live channel is `Ready` from the start, bound to its stand-in until the
  first frame arrives; it never holds a shot back (`usable` gates on files, not on a
  camera that might never deliver). `blocking_texture_loads` stays 0 while playing.
- **GL state:** the unpack-state rule from #22 applies, with a twist: this path *binds*
  `PIXEL_UNPACK_BUFFER` on purpose, so it must unbind it afterwards (file uploads and
  egui assume it unbound). A selftest pins both.
- **Errors:** an unplugged camera or a permission refusal shows in the panel ("camera
  'FaceTime HD Camera': access denied; allow it in System Settings → Privacy & Security →
  Camera"), the channel falls back to its stand-in, and the capture thread retries every
  couple of seconds. Live mode never quits because a device went away.

### Microphone texture

Shadertoy's audio input is a 512×2 single-channel 8-bit texture: row 0 the spectrum,
row 1 the waveform, sampled as `texture(iChannelN, vec2(x, 0.25))` and `vec2(x, 0.75)`.
Shadertoy builds it with the WebAudio `AnalyserNode` defaults, so we match those to make
pasted shaders behave:

- mono mix of the input, a 2048-sample window (Blackman, as the AnalyserNode uses), FFT
  with `realfft`, magnitudes smoothed over time with 0.8, converted to dB and mapped from
  [-100, -30] dB to [0, 1]; the first 512 of the 1024 bins (0–12 kHz at 48 kHz) go in
  row 0;
- row 1: the latest 512 samples, mapped from [-1, 1] to [0, 1];
- stored as `R8` (WebGL 2/GLSL ES 3.00 have it; `texture()` returns it in `.r`), filter
  `linear`, wrap `clamp`.

The `cpal` input callback only pushes samples into a `ringbuf` SPSC ring (no locks, no
allocation, the same rule as `fill`); an analysis step on the render thread (or a small
worker) reads the newest 2048 samples once per film frame, computes the 1 KB texture and
uploads it directly (1 KB needs no PBO). Latency is the window itself (2048 samples ≈
43 ms) plus the device buffer.

A **mic bus for sound shaders** (processing the voice) is a different problem: today's
soundtrack is synthesised ~0.7 s ahead in chunks, which is fine for a film and useless
for a voice. That's Phase 4 (see [Live sound](#live-sound-phase-4-explore)).

## Real-time processing

### Latency budget

From light hitting the sensor to the far end of a call, most of the delay isn't ours:

| stage | typical | ours? |
|---|---|---|
| camera exposure + driver delivery | 33–70 ms at 30 fps | no |
| capture thread: convert, publish | 2–5 ms | yes |
| wait for the next film frame (render at `movie.fps`) | 0–1 film frame | yes |
| PBO → texture, render | within the frame | yes |
| present (vsync, compositor) | 1 refresh | partly |
| output: window/OBS capture or Spout/Syphon; virtual camera | 0–2 frames | depends on route |
| meeting app encode, network, decode | 100–300 ms | no |

**Target:** shotglass adds at most **two film frames** between a camera frame arriving
and the processed frame leaving (≈67 ms at 30 fps, ≈33 ms at 60). The Performance panel
shows the measured age of the camera frame at present time (capture timestamp → swap),
so the owner can check it on the RTX 3060. Live films should use `fps = 30` or `60`,
matching what the camera delivers; rendering faster than the camera only repeats
frames.

Audio-visual sync: the meeting app sends the real mic (or the virtual cable) with its own
latency; delaying audio to match video is the meeting app's job and not attempted here.

### Time in live mode

- `iTime` runs from the wall clock (the `Clock`, which follows the sound card when sound
  plays), unbounded. Live films are looping films (#49): `iTime` wraps within the loop
  period without losing precision, and the soundtrack loops seamlessly. Live mode
  requires loop mode, or treats the film as one shot looping on its duration.
- No timeline scrubbing; the transport is play/pause and "next/previous shot" (cut
  between shots by key, using the film's transitions), which makes a film a set of live
  looks. *(Open question 9.)*
- Render once per film frame stays true; the auto scale keeps working (camera texture
  size doesn't change with the render scale).

### Output frame

- **Output size** is fixed per session (`--size 1280x720` default; meeting apps want
  720p or 1080p at 30 fps) and independent of the window size. The auto scale renders
  smaller and the composite scales up, as now.
- **Getting pixels out** for routes that need CPU memory (v4l2loopback, NDI, the Syphon
  CPU path, Phase 3 cameras): an **async readback**, the same pattern as sound: render,
  `glReadPixels` into the next PBO of a ring, fence, and collect it a frame later by
  polling oldest-first. That's the PBO readback follow-up of #1 (export can use it too).
  It adds one frame of latency and no sync. If the consumer is slow, frames are dropped
  at the output, never queued.
- **GPU-sharing routes** (Spout's GL backend, `SyphonOpenGLServer`) share the composite
  texture directly: no readback, no added frame.

## Invariants

What live mode changes, stated as the CLAUDE.md lines the Phase 1 PR would add or amend:

- **Deterministic frames (amended):** "A frame depends only on (movie, t, size, scale)"
  remains true for every offline path and for the player in normal mode. *Live mode is
  the exception, and only live mode:* there a frame also depends on the camera/mic
  samples, which are not reproducible. New line: **"Live inputs never reach offline
  paths."** `still`, `strip`, `export`, `wav`, `check` and the golden tests bind a live
  channel's `stand_in` (or black), so a film with a camera renders identically on every
  machine and in CI. The player outside live mode does the same, so editing a live film
  without a camera shows exactly what `still` shows.
- **Playback never blocks (extended):** never on a camera or mic either. Capture runs on
  its own threads, publishes the newest frame lock-free, and the render thread takes it
  if a PBO is free and skips it otherwise. A live channel is `Ready` from the start
  (stand-in), so it never gates a shot.
- **No per-frame GPU→CPU syncs (extended):** camera uploads and output readbacks use PBO
  rings with fences polled with a zero timeout, oldest-first, in the existing batches;
  device enumeration happens at startup and on request, never per frame.
- **The audio callback never blocks or allocates (extended to input):** the `cpal`
  input callback only writes to an SPSC ring.
- **Unpack state:** the camera path binds `PIXEL_UNPACK_BUFFER` and must unbind it and
  restore alignment/row length after each upload (as file uploads and the sound path do).
- **Render once per film frame:** unchanged; camera frames don't trigger renders.
- **The engine provides a contract, not looks:** the new channel kinds, the 512×2 mic
  layout and `iChannelTime` are contract (a minor bump, since `movie.toml` gains channel
  kinds; films without them are unaffected). Effects (chroma keys, edge detection, CRT
  looks) belong in the kit.

## Determinism

Summarising the above, because it's the invariant the issue asks about:

- **Offline is unchanged.** With live channels on stand-ins, every command except `live`
  produces bit-for-bit what it produces today for the same inputs. Golden tests can cover
  a live film (rendered from its stand-in) like any other.
- **Live mode is explicitly non-deterministic** and says so (title bar "LIVE", no
  export from the live view in Phases 1–3).
- **Reproducing a live session** is possible later by **recording a take**: the camera
  frames (as a video file, #32) and the mic (WAV), with timestamps. Replaying the take
  as the channel source makes `export` of a live performance deterministic again. That's
  Phase 4, and it reuses #32's video-channel path.

## Outputs

### Phase 1: window and full screen

- `shotglass live <film>` opens two views: the normal player UI (device pickers,
  Performance panel, shot list) and an **output window** showing only the frame, at the
  output size, with no UI. On a second monitor it can go full screen (F11 or
  `--fullscreen`), which is what screen sharing and projectors want. egui (eframe 0.37)
  has extra viewports and `ViewportCommand::Fullscreen`; whether the glow backend lets
  the second viewport draw our renderer's textures (shared GL context) is the second
  Phase 1 spike, with a fallback of one window that toggles between UI and output-only.
- **Meeting apps** can share the output window directly ("share window"), or the owner
  uses **OBS**: Window Capture of "shotglass output" → Start Virtual Camera. That gives a
  real virtual camera on Windows, macOS and Linux from Phase 1, with no code of ours. The
  README gets a recipe.
- **Audio out:** `--audio-out <name>` / `[live] audio_out` opens that `cpal` output device
  instead of the default; `shotglass devices` lists cameras, inputs and outputs. With a
  virtual cable as the output, the meeting app picks the cable as its microphone and
  hears the film's soundtrack (and, by setting the meeting app's mic to the real mic
  instead, the voice; mixing voice + soundtrack is the cable app's or OBS's job until
  Phase 4).

### Phase 2: bridges

- **Windows: Spout** with the GL backend of `spout2-rs` (or `spout-rs`; decide by a
  spike on the RTX 3060), sharing the composite texture each frame. Feature
  `spout`, on by default on Windows (vendored, no runtime files).
- **macOS: Syphon**, calling `SyphonOpenGLServer` through `objc2` for zero copy, linking
  the framework via `syphon-rs`'s vendored build (needs Xcode) or a framework the user
  installs. Feature `syphon`. OBS on macOS reads Syphon without plugins.
- **Linux: v4l2loopback** output through the `v4l` crate and the async readback (YUYV or
  RGB24, whichever the device negotiates). This is a virtual camera already, no OBS
  needed. Feature `v4l2loopback`.
- **NDI (optional feature `ndi`, off by default):** `grafton-ndi` with the user-installed
  runtime, for multi-machine setups or a single cross-platform path. Needs the owner's OK
  on the proprietary runtime and its trademark/attribution terms.
- One trait in a new `live/out.rs`: `trait LiveOutput { fn send(&mut self, frame:
  OutFrame) }` where `OutFrame` is either a GL texture (shared routes) or a CPU buffer
  from the readback ring; several outputs can run at once (window + Spout).

### Phase 3: native virtual cameras (conditional)

Only if the owner wants live mode for people who won't install OBS:

- **Windows 11:** a media-source DLL (Rust with the `windows` crate implementing
  `IMFMediaSource`, `IMFMediaStream`, `IMFAttributes`, or a vendored C++ sample after
  checking its licence), registered per user, created with `MFCreateVirtualCamera(..,
  MFVirtualCameraLifetime_Session, MFVirtualCameraAccess_CurrentUser, ..)` so the camera
  exists while shotglass runs. Frames cross into the Frame Server process through a named
  shared-memory ring (or a shared D3D11 texture via `WGL_NV_DX_interop`). Windows 10
  users stay on OBS/Spout.
- **macOS:** a Camera Extension target (Swift or Rust with `objc2-core-media-io`) inside
  a `Shotglass.app` bundle, with a sink stream the app writes to and a source stream
  meeting apps read; installed with `OSSystemExtensionRequest`; Developer ID signed,
  notarised, entitlements and app group. This also forces the app-bundle questions
  (#28 prebuilt binaries) to be answered first.
- Linux needs nothing more (v4l2loopback from Phase 2; PipeWire node later if browsers
  move to it).

### Live sound (Phase 4, explore)

- A **low-latency block path** next to the chunked soundtrack: blocks of 256–1024
  samples, synthesised on the GPU as the mic delivers them (the async PBO + fence path,
  polled every frame), mixed into the output. Sound shaders get the mic as `sg_mic` (a
  1-D texture like `sg_music`). The real issue is the GPU round trip: one block per
  rendered frame means ≈ one frame of audio latency plus device buffers, i.e. 25–50 ms,
  acceptable for effects, marginal for monitoring your own voice.
- "Sound is identical whatever the chunk size" would hold *given the same input*;
  recorded takes make that testable.
- **Takes:** record camera + mic during a live session, replay them as channel sources
  for a deterministic `export` (needs #32's video channels).

## Engine changes per phase

### Phase 1: camera + mic in, window out

| file | change |
|---|---|
| `Cargo.toml` | `nokhwa` (no default features, `input-native`), `realfft`, `ringbuf` |
| `movie.rs` | channel kinds `camera`/`mic` with `size`, `fps`, `mirror`, `stand_in`; validation; unit tests; proptest generator; `referenced_files` gets the stand-ins |
| `library.rs` | `movie.local.toml` gains a `[live]` table (camera, mic, audio_out) |
| new `live/mod.rs`, `live/camera.rs`, `live/mic.rs` | capture threads, device listing, conversion, newest-frame slot, mic ring + analysis (FFT/waveform), a **fake source** (synthetic frames/samples) for tests |
| `gfx.rs` | live textures: stand-in until the first frame, PBO upload ring with fences, `iChannelTime`, unpack-buffer unbinding, Performance timing; no changes to file textures |
| `audio.rs` | `AudioOut::open(device: Option<&str>)`; a mic input stream next to the output |
| `app.rs` | `live` mode: output viewport (borderless/full screen), device pickers, camera-age readout, shot switching keys, no scrubbing |
| `main.rs` | `live` and `devices` commands; offline commands bind stand-ins |
| `validate.rs` / `check` | validate live channels; the "reads iChannelN but binds nothing" lint counts live channels as bound |
| `selftest.rs` | checks below |
| `tests/golden.rs` | a small live test film rendered from its stand-in |
| README, CLAUDE.md, template | live mode section and OBS recipe; invariants above; commented example |

**Selftest checks** (each mutation-checked): with the fake camera, playback does 0
blocking texture loads and 0 waits on camera fences; the uploaded texture holds the
newest published frame, not an older one; `PIXEL_UNPACK_BUFFER` is unbound and unpack
state restored after a camera upload; offline rendering of a live film equals rendering
with the stand-in file bound as a normal texture (bit for bit); the mic texture of a
known sine has its peak in the expected bin and the waveform row matches.

Owner checks: the MacBook's built-in camera at 720p30 (permission prompt from Terminal),
the RTX 3060 with a USB webcam, camera age ≤ 2 frames in the Performance panel, OBS
window capture → virtual camera → a test call.

### Phase 2: bridges

| file | change |
|---|---|
| `gfx.rs` | async output readback ring (PBO + fence, the #1 follow-up), shared-texture hand-off |
| new `live/out.rs` + `live/out/{spout,syphon,v4l2}.rs` | `LiveOutput` trait and implementations behind cargo features |
| `Cargo.toml` | `spout2-rs`/`spout-rs` (Windows), `syphon-rs` or direct `objc2` (macOS), `v4l` (Linux), optional `grafton-ndi` |
| `app.rs`, `main.rs` | `--out spout\|syphon\|v4l2:/dev/video10\|ndi`, output status |
| `selftest.rs` | readback ring: no blocking reads while live, frames delivered in order, dropped (not queued) when the consumer stalls; per-backend smoke tests where CI can run them (v4l2loopback isn't loadable in CI containers: manual) |
| CI | Windows and macOS builds with the features on (build only) |

### Phase 3: native virtual cameras

New workspace members (`shotglass-vcam-win` DLL, `shotglass-vcam-mac` extension), the
macOS app bundle and signing pipeline, install/uninstall commands, and their docs.
Nothing in the render path beyond Phase 2's `LiveOutput`.

### Phase 4: live sound and takes (explore)

`sound.rs`/`gfx.rs`/`audio.rs`: the low-latency block path and `sg_mic`; recording takes
(depends on #32 video channels). Each a design note first.

## Later

- GPU YUV→RGB conversion (NV12/YUYV planes) for 1080p60 cameras.
- More than one camera; camera controls (exposure, focus) through nokhwa's controls API.
- Segmentation/face-mesh masks as extra channels (a CPU/ML pass; heavy, separate design).
- PipeWire camera node on Linux (`pipewire-vircam`) once browsers read PipeWire cameras
  by default.
- The film's **own soundtrack** as a Shadertoy-style FFT channel (`{ audio = "soundtrack"
  }`): deterministic, since the soundtrack is, and useful for music films outside live
  mode. It needs scenes to wait for sound chunks, so it's its own small design.
- Web player: `getUserMedia` for camera/mic (nokhwa has a wasm backend) behind the same
  channel kinds.

## Open questions for the owner

1. **What's the main use?** Effects on your own face in calls (camera matters most), or
   visuals for streaming/VJ (output and sound matter most)? It decides whether Phase 2
   or Phase 4 comes second.
2. **Is OBS acceptable as the bridge to a virtual camera** on Windows and macOS? If yes,
   Phase 3 probably never happens.
3. **Apple Developer Program** ($99/year, Developer ID, notarisation): only needed for
   Phase 3 on macOS (and for distributing a signed app at all, #28). Willing?
4. **NDI:** OK to support an optional feature that needs NDI's proprietary runtime, or
   Spout/Syphon/v4l2loopback only?
5. **Syntax:** camera/mic as `[textures]` entries (proposed), or a separate `[inputs]`
   table? Device names only in `movie.local.toml`/CLI (proposed)?
6. **Mirroring default:** `false` (readable text for the other side, proposed) or `true`
   (what you see in your own preview)?
7. **Stand-ins:** default black, or require a `stand_in` image so offline renders of live
   films are meaningful?
8. **Which Mac** (model, macOS version)? cpal's CoreAudio backend wants macOS 14.2+;
   Camera Extensions need 12.3+; Continuity Camera (an iPhone as a webcam) works through
   the same AVFoundation path.
9. **Shot switching:** should live mode cut between a film's shots by key (a film as a
   set of live looks), or play one looping shot?
10. **Voice processing** (Phase 4): needed, or is the mic only a visual input?
