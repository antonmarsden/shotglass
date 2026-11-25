void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float inside = step(0.6, uv.x) * step(uv.x, 0.95) * step(0.55, uv.y) * step(uv.y, 0.95);
  fragColor = vec4(1.0, 1.0, 1.0, inside);
}
