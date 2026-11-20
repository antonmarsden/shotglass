// ============================================================
//  BIG SHADOW — music. Runs across the cuts; `time` is movie time.
//  Shot-specific sounds live in sfx/ and are attached to shots in movie.toml.
// ============================================================
#include "sfx/common.glsl"

// fades a section in over [a0,a1] and out over [b0,b1]
float window(float t, float a0, float a1, float b0, float b1){
  return smoothstep(a0, a1, t) * (1.0 - smoothstep(b0, b1, t));
}

// struck bells with inharmonic partials, on a slow pentatonic walk
vec2 bells(float t, float t0, float step, int seed){
  vec2 s = vec2(0.0);
  float local = t - t0;
  if (local < 0.0) return s;
  int n = int(local / step);
  for (int k = 0; k <= 4; k++){
    int i = n - k;
    if (i < 0) break;
    if (rnd(i * 7 + seed) < 0.25) continue;                 // rests
    float dt = local - float(i) * step;
    float f = note(penta(5 + int(rnd(i * 7 + seed + 1) * 8.0)), 220.0);
    float env = exp(-dt * 1.6) * smoothstep(0.0, 0.004, dt);
    float b = sin(TAU * f * dt) + 0.35 * sin(TAU * f * 2.76 * dt) * exp(-dt * 3.0)
            + 0.15 * sin(TAU * f * 5.40 * dt) * exp(-dt * 6.0);
    float pan = rnd(i * 7 + seed + 2);
    s += b * env * vec2(1.0 - 0.6 * pan, 0.4 + 0.6 * pan);
  }
  return s;
}

vec2 mainSound(int samp, float time){
  float t = time;
  vec2 o = vec2(0.0);

  // wall: a low drone that swells as the shadow grows and sinks when the flame dies
  float wall = window(t, CUE_WALL_START, CUE_WALL_START + 1.5, CUE_MOON_START, CUE_MOON_SETTLED);
  float grow = smoothstep(CUE_WALL_START, CUE_WALL_START + 6.0, t);
  float dying = smoothstep(CUE_WALL_START + 6.4, CUE_WALL_START + 8.2, t);
  float dr = drone(t, 55.0) * (0.10 + 0.12 * grow) * mix(1.0, 0.4, dying);
  o += vec2(dr, dr * 0.95) * wall;

  // moon: slow bells over a soft fifth (mix.glsl adds the echo)
  float moon = window(t, CUE_MOON_START, CUE_MOON_SETTLED + 0.6, CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED + 0.5);
  o += bells(t, CUE_MOON_START + 0.4, 0.62, 17) * 0.11 * moon;
  float pad = drone(t, 73.42) * 0.06 + drone(t, 110.0) * 0.04;
  o += vec2(pad * 0.9, pad) * moon;

  // close-up and end: the drone returns, warmer
  float close = window(t, CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED, CUE_END_SETTLED, MOVIE_DURATION);
  o += vec2(drone(t, 55.0) * 0.08) * close;

  return o;
}
