// One low thump as the closing iris shuts. The iris finishes when the transition
// into this shot has settled.
#include "common.glsl"

vec2 mainSound(int samp, float time){
  float dt = time - (CUE_END_SETTLED - CUE_END_START);
  if (dt < 0.0) return vec2(0.0);
  // pitch falls from 112 Hz to 42 Hz: phase is the integral of 42 + 70·e^(-6t)
  float phase = TAU * (42.0 * dt + 70.0 / 6.0 * (1.0 - exp(-dt * 6.0)));
  return vec2(sin(phase)) * exp(-dt * 2.6) * 0.45 * smoothstep(0.0, 0.005, dt);
}
