<!-- See CONTRIBUTING.md. One issue per PR. -->

Closes #

**What changes**


**How it was tested** (and on what: OS, GPU or software OpenGL)


## Checklist

- [ ] Added or updated a test (unit / property test, selftest check for renderer behaviour — mutation-checked: it fails when the code is broken — or a golden case for a new look)
- [ ] `cargo fmt && cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] `cargo run -- check movies/big-shadow --web --no-gpu` passes, and so does every other shader this PR adds (demo, template, transitions, selftest fixtures)
- [ ] Golden references: unchanged, or re-blessed in this commit because the change is *meant* to look or sound different (and I looked at the new PNGs / listened to the WAVs)
- [ ] README.md + docs/ (user docs) and CLAUDE.md (map, invariants) updated if what they describe changed
- [ ] A new `movie.toml` field is validated in `Movie::parse`, unit-tested, and in the docs (`docs/movies.md`) and `templates/new/movie.toml`
- [ ] No new built-in GLSL (reusable GLSL goes in [shotglass-kit](https://github.com/antonmarsden/shotglass-kit))

## Invariants touched

Tick each one this change could affect and say how it still holds (see [CLAUDE.md](https://github.com/antonmarsden/shotglass/blob/main/CLAUDE.md#invariants-each-was-a-real-bug-most-have-a-test)). Leave the rest unticked.

- [ ] Playback never blocks on a compile
- [ ] Render once per film frame while playing; auto scale driven by GPU time only (stalls aren't drops)
- [ ] No per-frame GPU→CPU syncs on the render path (glGetError, read_pixels, finish, unready queries; sound streams via PBO + fence)
- [ ] The audio callback (`fill`) never blocks or allocates
- [ ] `Clock::time()` never decreases while playing
- [ ] A broken edit keeps the last working version on screen
- [ ] Sound is identical whatever the chunk size; banded = single draw; streamed = blocking, bit for bit
- [ ] Deterministic frames: a frame depends only on (movie, t, size, scale); frame n is at `frame_time(n)`
- [ ] Readbacks restore GL pack state and check bounds first
- [ ] Shaders compile as `#version 330 core` / `300 es` unless the film opts in with `glsl`
- [ ] The engine provides a contract, not looks (uniforms, entry points, transition interface, cue defines are semver API)

**Needs a real-GPU test by the owner:** yes / no
