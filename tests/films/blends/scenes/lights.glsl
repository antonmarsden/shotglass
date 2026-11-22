// Dark blue field with a few moving bright dots: something for the bloom to catch.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - 0.5 * iResolution.xy) / iResolution.y;
  vec3 col = vec3(0.04, 0.06, 0.14) + 0.06 * vec3(p.y + 0.5);
  for (int i = 0; i < 5; i++) {
    float a = float(i) * 1.2566 + iTime * 0.4;
    vec2 c = 0.32 * vec2(cos(a), sin(a * 1.3));
    float d = length(p - c);
    col += vec3(1.0, 0.8 - 0.1 * float(i), 0.5) * smoothstep(0.035, 0.02, d);
  }
  fragColor = vec4(col, 1.0);
}
