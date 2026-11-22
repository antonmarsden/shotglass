// The wrapped, centred paragraph (iChannel6) above a mixed right-to-left line
// (iChannel7), each drawn at its texel size scaled to fit, coverage only.
vec3 over(vec3 col, sampler2D t, vec2 tsize, vec2 p, vec2 centre, float scale, vec3 ink) {
  vec2 uv = (p - centre) / (tsize * scale) + 0.5;
  if (any(lessThan(uv, vec2(0.0))) || any(greaterThan(uv, vec2(1.0)))) return col;
  return mix(col, ink, texture(t, uv).a);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 res = iResolution.xy;
  vec3 col = vec3(0.92, 0.9, 0.85);
  float s6 = min(res.x * 0.9 / iChannelResolution[6].x, res.y * 0.6 / iChannelResolution[6].y);
  float s7 = res.x * 0.8 / iChannelResolution[7].x;
  col = over(col, iChannel6, iChannelResolution[6].xy, fragCoord, vec2(0.5, 0.62) * res, s6, vec3(0.1, 0.1, 0.15));
  col = over(col, iChannel7, iChannelResolution[7].xy, fragCoord, vec2(0.5, 0.16) * res, s7, vec3(0.55, 0.1, 0.1));
  fragColor = vec4(col, 1.0);
}
