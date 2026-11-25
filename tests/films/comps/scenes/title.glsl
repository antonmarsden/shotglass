void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float band = step(0.4, uv.y) * step(uv.y, 0.6) * step(0.2, uv.x) * step(uv.x, 0.8);
  fragColor = vec4(1.0, 1.0, 0.9, 0.8 * band);
}
