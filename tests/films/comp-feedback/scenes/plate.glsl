// The film's plate: a dark, smooth diagonal gradient that drifts slowly (no wrap, so no
// seam), so a held comp shows over a moving background.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float g = 0.06 + 0.05 * (0.5 + 0.5 * sin(3.0 * (uv.x + uv.y * 0.5) + iTime * 1.5));
  fragColor = vec4(g * 1.1, g, g * 1.4, 1.0);
}
