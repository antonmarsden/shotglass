// Playful pizzicato for the close-up. It has sound_lead = 0.6 in movie.toml, so it
// starts before the iris opens (a J-cut): `time` is negative during the lead.
#include "common.glsl"

vec2 mainSound(int samp, float time){
  vec2 s = vec2(0.0);
  float local = time + 0.6;          // count from the start of the lead
  if (local < 0.0) return s;
  const float STEP = 0.3;
  int n = int(local / STEP);
  for (int k = 0; k <= 2; k++){
    int i = n - k;
    if (i < 0) break;
    if ((i & 3) == 3 && rnd(i + 99) < 0.6) continue;
    float dt = local - float(i) * STEP;
    float f = note(penta(int(rnd(i * 11 + 5) * 7.0)), 110.0);
    float env = exp(-dt * 9.0) * smoothstep(0.0, 0.002, dt);
    float p = sin(TAU * f * dt) + 0.5 * sin(TAU * 2.0 * f * dt) * exp(-dt * 8.0)
            + 0.25 * sin(TAU * 3.0 * f * dt) * exp(-dt * 14.0);
    float pan = 0.5 + 0.35 * sin(float(i) * 1.7);
    s += p * env * vec2(1.0 - pan, pan) * 0.15;
  }
  return s;
}
