// Rings that move outwards by two spacings per loop, so they line up again at the seam.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - 0.5 * iResolution.xy) / iResolution.y;
  float k = fract(length(p) * 6.0 - 2.0 * iMovieTime / MOVIE_DURATION);
  float ring = smoothstep(0.4, 0.5, k) * smoothstep(0.9, 0.8, k);
  fragColor = vec4(mix(vec3(0.1, 0.3, 0.5), vec3(0.9, 0.95, 1.0), ring), 1.0);
}
