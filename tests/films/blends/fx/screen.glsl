// Screen blend: 1 - (1 - base)(1 - layer), mixed by the layer's coverage × opacity.
vec4 blend(vec2 uv) {
  vec4 b = getBase(uv);
  vec4 l = getLayer(uv);
  vec3 s = 1.0 - (1.0 - b.rgb) * (1.0 - l.rgb / max(l.a, 1e-5));
  return vec4(mix(b.rgb, s, l.a), 1.0);
}
