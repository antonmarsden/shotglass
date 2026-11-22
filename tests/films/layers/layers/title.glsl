// A rounded banner with straight alpha (the footer premultiplies and applies the fades).
// Its own clock: iShotTime is layer-local, so the stripe slides from the layer's start.
float box(vec2 p, vec2 b, float r) {
  vec2 q = abs(p) - b + r;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - vec2(0.5, 0.3) * iResolution.xy) / iResolution.y;
  float d = box(p, vec2(0.42, 0.09), 0.04);
  float a = 1.0 - smoothstep(-0.002, 0.002, d);
  float slide = step(fract(p.x * 4.0 - iShotTime * 0.5), 0.15);
  vec3 col = mix(uTint, vec3(1.0), slide * 0.5);
  fragColor = vec4(col, a * 0.9);
}
