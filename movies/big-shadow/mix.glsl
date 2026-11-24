// ============================================================
//  BIG SHADOW — final mix. Runs after every track is synthesised and can read
//  them at any sample: music(samp), sfx(samp) (all shot sounds, already faded
//  for transitions). The effects come from lib/sound/mix.glsl.
// ============================================================
#include <sound/mix.glsl>

vec2 mainMix(int samp, float time){
  int rate = int(iSampleRate);

  // music with a ping-pong echo (310 ms, then 620 ms at half level), mostly heard
  // on the moon's bells
  float echo = smoothstep(CUE_MOON_START, CUE_MOON_SETTLED, time) * (1.0 - smoothstep(CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED, time));
  vec2 m = music(samp) + 0.35 * echo * kit_mix_delay(kit_bus_music, samp, rate, 0.31, 2, 0.5, 1.0);

  // sound effects plus a little room (reflections up to 131 ms)
  vec2 fx = sfx(samp) + 0.2 * kit_mix_reverb(kit_bus_sfx, samp, rate, 0.131, 0.17);

  // duck the music under loud effects (the iris thump)
  vec2 o = m * kit_mix_duck(samp, rate, 0.12, 0.35, 0.7) + fx;
  o *= smoothstep(0.0, 0.3, time) * (1.0 - smoothstep(MOVIE_DURATION - 0.4, MOVIE_DURATION, time));
  return kit_soft_clip(o * 1.08, 0.9);
}
