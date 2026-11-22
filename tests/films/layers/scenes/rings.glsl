void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - 0.5 * iResolution.xy) / iResolution.y;
  float r = length(p);
  float v = 0.5 + 0.5 * cos(r * 40.0 - iTime * 3.0);
  fragColor = vec4(0.2 + 0.6 * v, 0.25, 0.35 + 0.3 * (1.0 - v), 1.0);
}
