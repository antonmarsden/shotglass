// Every frequency makes a whole number of cycles in MOVIE_DURATION (2 s), so the end
// of the loop runs into its start without a click.
vec2 mainSound(int samp, float time) {
  float swell = 0.6 + 0.4 * cos(6.2831853 * time / MOVIE_DURATION);
  float s = sin(6.2831853 * 110.0 * time) + 0.5 * sin(6.2831853 * 165.0 * time);
  return 0.12 * swell * s * vec2(1.0, 0.9);
}
