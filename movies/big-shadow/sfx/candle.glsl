// The candle on the wall shot. `time` is shot-local, and the shot's uniforms
// (uLight, uDist, ...) are available here too, so the crackle dies with the flame.
#include "common.glsl"

// sparse little clicks
float crackle(int samp, float t){
  float v = 0.0;
  const float RATE = 70.0;
  int cell = int(t * RATE);
  for (int c = cell - 2; c <= cell; c++){
    if (kit_hash(c * 3 + 1) < 0.18){
      float t0 = (float(c) + kit_hash(c * 3 + 2)) / RATE;
      float dt = t - t0;
      if (dt > 0.0) v += (kit_hash(samp) * 2.0 - 1.0) * exp(-dt * 380.0) * (0.3 + 0.7 * kit_hash(c * 3 + 3));
    }
  }
  return v;
}

vec2 mainSound(int samp, float time){
  float t = max(time, 0.0);
  // a thin uneasy beating tone that grows as the light gets closer (uDist shrinks)
  float near = smoothstep(0.95, 0.22, uDist);
  float uneasy = (sin(TAU * 440.0 * t) + sin(TAU * 443.0 * t)) * 0.012 * near * near;
  float c = crackle(samp, t) * 0.10;
  return (vec2(c) + vec2(0.8, 1.0) * uneasy) * uLight;
}
