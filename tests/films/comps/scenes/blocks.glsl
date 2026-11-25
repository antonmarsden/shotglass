// An integer hash, not fract(sin(...) * 43758.5453): built-in sin is ~5e-4 off on the
// Apple Software Renderer, and that factor turns the error into different blocks.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 c = floor(uv * vec2(8.0, 4.0));
  uint h = uint(c.x) * 73856093u ^ uint(c.y) * 19349663u;
  h ^= h >> 13u;
  h *= 0x5bd1e995u;
  h ^= h >> 15u;
  float on = float((h >> 7u) & 1u);
  vec2 f = fract(uv * vec2(8.0, 4.0));
  float inset = step(0.1, f.x) * step(f.x, 0.9) * step(0.1, f.y) * step(f.y, 0.9);
  fragColor = vec4(vec3(on * inset), 1.0);
}
