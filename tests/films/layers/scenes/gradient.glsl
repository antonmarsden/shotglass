void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  fragColor = vec4(uv.x, 0.3 + 0.2 * uv.y, 1.0 - uv.x, 1.0);
}
