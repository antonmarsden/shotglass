// The background plate: a dark diagonal gradient with a faint grid, so a smear of the
// whole composite would show if the plate moved (it doesn't).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 c = mix(vec3(0.04, 0.05, 0.10), vec3(0.10, 0.12, 0.20), 0.5 * (uv.x + uv.y));
  vec2 g = abs(fract(uv * vec2(8.0, 4.5)) - 0.5);
  c += 0.03 * step(0.46, max(g.x, g.y));
  fragColor = vec4(c, 1.0);
}
