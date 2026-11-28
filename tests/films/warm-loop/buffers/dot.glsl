// The element on transparent: a dot on a closed figure-eight, once per period
// (MOVIE_DURATION), so the loop is seamless.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  float a = 6.28318530718 * iMovieTime / MOVIE_DURATION;
  vec2 p = fragCoord / iResolution.y;
  vec2 c = vec2(0.889 + 0.55 * sin(a), 0.5 + 0.28 * sin(2.0 * a));
  float d = smoothstep(0.075, 0.055, length(p - c));
  fragColor = vec4(d);
}
