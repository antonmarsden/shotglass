// A 400 ms echo of everything. At the start of each loop it repeats the end of the
// previous one: the mix reads across the seam.
vec2 mainMix(int samp, float time) {
  int d = int(iSampleRate * 0.4);
  vec2 dry = music(samp) + sfx(samp);
  vec2 wet = music(samp - d) + sfx(samp - d);
  return dry + 0.45 * wet.yx;
}
