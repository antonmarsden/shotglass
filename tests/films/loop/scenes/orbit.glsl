// A dot that orbits once per loop: iMovieTime / MOVIE_DURATION runs 0 -> 1 each period.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - 0.5 * iResolution.xy) / iResolution.y;
  float a = 6.2831853 * iMovieTime / MOVIE_DURATION;
  vec2 c = 0.3 * vec2(cos(a), sin(a));
  float d = length(p - c);
  vec3 col = vec3(0.08, 0.1, 0.16) + 0.12 * vec3(0.5 + 0.5 * cos(a + p.x * 4.0));
  col = mix(col, vec3(1.0, 0.8, 0.3), smoothstep(0.08, 0.06, d));
  fragColor = vec4(col, 1.0);
}
