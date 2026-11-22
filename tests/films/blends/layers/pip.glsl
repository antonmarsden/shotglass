// Picture-in-picture: the picture track (@picture, iChannel0) in the top-right corner,
// framed; transparent elsewhere.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 lo = vec2(0.62, 0.62), hi = vec2(0.96, 0.96);
  vec2 frame = 2.0 / iResolution.xy;
  if (any(lessThan(uv, lo - frame)) || any(greaterThan(uv, hi + frame))) {
    fragColor = vec4(0.0);
    return;
  }
  bool inside = all(greaterThanEqual(uv, lo)) && all(lessThanEqual(uv, hi));
  vec3 c = inside ? texture(iChannel0, (uv - lo) / (hi - lo)).rgb : vec3(0.95);
  fragColor = vec4(c, 1.0);
}
