// A warm paper texture, multiply-blended (fx/multiply.glsl) over the middle of the film.
float hash(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 g = floor(fragCoord / 3.0);
  float n = 0.85 + 0.15 * hash(g);
  float stripe = 0.92 + 0.08 * step(0.5, fract(fragCoord.y / 24.0));
  fragColor = vec4(vec3(1.0, 0.86, 0.66) * n * stripe, 1.0);
}
