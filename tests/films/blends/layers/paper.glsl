// A warm paper texture, multiply-blended (fx/multiply.glsl) over the middle of the film.
// Integer hash, exact on every driver (a sin-based one depends on the driver's sin).
float hash(vec2 p) {
  uvec2 q = uvec2(ivec2(p));
  uint h = q.x * 1597334677u ^ q.y * 3812015801u;
  h = (h ^ (h >> 16)) * 2246822519u;
  h ^= h >> 13;
  return float(h >> 8) / 16777216.0;
}
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 g = floor(fragCoord / 3.0);
  float n = 0.85 + 0.15 * hash(g);
  float stripe = 0.92 + 0.08 * step(0.5, fract(fragCoord.y / 24.0));
  fragColor = vec4(vec3(1.0, 0.86, 0.66) * n * stripe, 1.0);
}
