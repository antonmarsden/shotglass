// The film's plate: a dark diagonal gradient that drifts, so a held comp shows over a
// moving background.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float g = 0.08 + 0.12 * fract(uv.x + uv.y * 0.5 + iTime * 0.2);
  fragColor = vec4(g, g * 0.8, g * 0.6, 1.0);
}
