// Playful pizzicato for the close-up (bs_inst_pluck). It has sound_lead = 0.6 in movie.toml, so it
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
    if ((i & 3) == 3 && bs_hash(i + 99) < 0.6) continue;
    float dt = local - float(i) * STEP;
    float f = penta_hz(int(bs_hash(i * 11 + 5) * 7.0), 45);   // from A2, 110 Hz
    float p = bs_inst_pluck(dt, f, 0.6, 0.35);   // silent after 0.6 s: the loop keeps 2 steps
    float pan = 0.5 + 0.35 * sin(float(i) * 1.7);
    s += p * vec2(1.0 - pan, pan) * 0.15;
  }
  return s;
}
