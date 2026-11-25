void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float k = step(0.5, fract(uv.x * 8.0 + iShotTime));
  // Comp cues are comp-local: the stripes darken towards the comp's end.
  float fade = 1.0 - 0.5 * clamp(iMovieTime / COMP_DURATION, 0.0, 1.0);
  fragColor = vec4(vec3(0.8, 0.3, 0.4) * mix(0.4, 1.0, k) * fade, 1.0);
}
