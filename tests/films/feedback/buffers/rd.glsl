// One Gray-Scott step on the previous state (iChannel0, read texel by texel); blue is
// the buffer-local iFrame (frames since the reset).
vec2 state(ivec2 p) {
  ivec2 r = ivec2(iResolution.xy);
  return texelFetch(iChannel0, clamp(p, ivec2(0), r - 1), 0).xy;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  ivec2 p = ivec2(fragCoord);
  vec2 c = state(p);
  vec2 lap = state(p + ivec2(1, 0)) + state(p - ivec2(1, 0)) + state(p + ivec2(0, 1)) +
             state(p - ivec2(0, 1)) - 4.0 * c;
  float a = c.x;
  float b = c.y;
  float abb = a * b * b;
  a += 0.2 * lap.x - abb + 0.037 * (1.0 - a);
  b += 0.1 * lap.y + abb - (0.06 + 0.037) * b;
  fragColor = vec4(clamp(a, 0.0, 1.0), clamp(b, 0.0, 1.0), float(iFrame) / 15.0, 1.0);
}
