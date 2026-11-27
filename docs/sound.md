# Sound

Music, per-shot sound and the mix pass, all synthesised on the GPU in GLSL.

[README](../README.md) · [Movies](movies.md) · [Channels](channels.md) · [Layers and blends](layers.md) · [Feedback buffers](buffers.md) · [Sound](sound.md) · [Loop films](loop-films.md) · [The player](player.md) · [Projects and libraries](libraries.md) · [Testing GLSL and quality metrics](testing-and-quality.md) · [Command line and export](cli.md) · [Testing shotglass](testing.md)

## Sound

All sound is GLSL, synthesised on the GPU when the movie loads. It comes in three layers:

```
music.glsl   movie time, runs across cuts ───────────────► music() ─┐
shot sounds  shot time + shot uniforms, faded per transition ► sfx() ─┼─► mix.glsl ─► speakers
                                                                     (or music + sfx)
```

**Music** (`[sound] music`) is `vec2 mainSound(int samp, float time)` with `time` in
movie seconds. Use the cue `#define`s to place it against the edit:

```glsl
vec2 mainSound(int samp, float time) {
    float k = smoothstep(CUE_MOON_START, CUE_MOON_SETTLED, time);   // follow the fade
    float s = 0.2 * sin(6.2831 * mix(110.0, 146.8, k) * time);
    return vec2(s);
}
```

**Shot sounds** (`sound = "sfx/candle.glsl"` on a shot) use the same signature, but
`time` is shot-local (like `iTime`) and the shot's uniforms are available, evaluated
at that time. A candle's sound that multiplies by the `uLight` its scene uses dies
exactly when the flame does. Shot sounds move with their shot when you retime the
edit. During a transition the outgoing and incoming sounds crossfade, using the
transition's ease and equal-power gains. `sound_lead` starts a sound before its
shot (a J-cut; `time` is negative during the lead), and `sound_tail` lets it ring
on after the shot. In both cases the sound skips the fade on that side.
`sound_gain` scales it.

**The mix** (`[sound] mix`) is optional: `vec2 mainMix(int samp, float time)` runs
after everything else and can read any sample of the two buses. That makes
delays, echoes and multi-tap reverbs cheap lookups, which a single `mainSound`
can't do:

```glsl
vec2 mainMix(int samp, float time) {
    int ms = int(iSampleRate * 0.001);
    vec2 echo = music(samp - 310 * ms) * 0.35;                       // 310 ms echo
    float duck = 1.0 - 0.7 * smoothstep(0.12, 0.35, sfxLevel(samp)); // music dips under loud effects
    return (music(samp) + echo) * duck + sfx(samp);
}
```

The mix can read up to `mix_lookback` seconds back (default 1) and 50 ms ahead.
Reads outside that range return silence. For ready-made effects (multi-tap delay,
reverb, smoothed ducking, a look-ahead limiter and soft clip), include shotglass-kit's
`<sound/mix.glsl>`.

Sound is synthesised in chunks of about 0.7 s, just ahead of the playhead (see
[Long films](player.md#long-films)). Each chunk of each track is cached by its source and
timing. Editing one shot's sound re-synthesises only that shot (and the mix), and
picture-only edits don't touch the sound. Until the new chunks are ready, the
previous version keeps playing. Recursive filters (resonant sweeps, feedback reverb) are the main
thing GLSL sound can't do cheaply. Multi-tap delays in the mix get you most of
the way.
