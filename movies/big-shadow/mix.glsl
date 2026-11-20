// ============================================================
//  BIG SHADOW — final mix. Runs after every track is synthesised and can read
//  them at any sample: music(samp), sfx(samp) (all shot sounds, already faded
//  for transitions) and sfxLevel(samp) for ducking.
// ============================================================

// a small room: a handful of short delays
vec2 room(int samp, int ms){
  vec2 r = vec2(0.0);
  const int TAPS = 6;
  int d[TAPS] = int[](23, 37, 53, 71, 97, 131);
  for (int k = 0; k < TAPS; k++){
    vec2 v = sfx(samp - d[k] * ms);
    r += (k % 2 == 0 ? v.xy : v.yx) * (0.35 / float(k + 1));
  }
  return r;
}

vec2 mainMix(int samp, float time){
  int ms = int(iSampleRate * 0.001);

  // music with a ping-pong echo, mostly heard on the moon's bells
  vec2 m = music(samp);
  vec2 e1 = music(samp - 310 * ms);
  vec2 e2 = music(samp - 620 * ms);
  float echo = smoothstep(CUE_MOON_START, CUE_MOON_SETTLED, time) * (1.0 - smoothstep(CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED, time));
  m += (vec2(e1.y, e1.x) * 0.35 + e2 * 0.18) * echo;

  // sound effects plus a little room
  vec2 fx = sfx(samp) + room(samp, ms) * 0.5;

  // duck the music under loud effects (the iris thump)
  float duck = 1.0 - 0.7 * smoothstep(0.12, 0.35, sfxLevel(samp));

  vec2 o = m * duck + fx;
  o *= smoothstep(0.0, 0.3, time) * (1.0 - smoothstep(MOVIE_DURATION - 0.4, MOVIE_DURATION, time));
  return tanh(o * 1.2) * 0.9;
}
