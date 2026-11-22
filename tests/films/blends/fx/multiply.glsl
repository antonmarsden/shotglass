// Multiply blend: base × layer, mixed by the layer's coverage × opacity.
vec4 blend(vec2 uv) {
  vec4 b = getBase(uv);
  vec4 l = getLayer(uv);
  vec3 m = b.rgb * l.rgb / max(l.a, 1e-5);
  return vec4(mix(b.rgb, m, l.a), 1.0);
}
