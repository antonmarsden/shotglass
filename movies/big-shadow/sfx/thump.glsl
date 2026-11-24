// One low thump as the closing iris shuts. The iris finishes when the transition
// into this shot has settled.
#include "common.glsl"

vec2 mainSound(int samp, float time){
  float dt = time - (CUE_END_SETTLED - CUE_END_START);
  if (dt < 0.0) return vec2(0.0);
  // a deep kick that settles at 42 Hz and rings out for 1.5 s
  return vec2(bs_drum_kick(dt, 42.0, 1.5)) * 0.45;
}
