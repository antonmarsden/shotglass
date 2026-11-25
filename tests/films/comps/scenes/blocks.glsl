void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 c = floor(uv * vec2(8.0, 4.0));
  float on = step(0.5, fract(sin(dot(c, vec2(12.9898, 78.233))) * 43758.5453));
  vec2 f = fract(uv * vec2(8.0, 4.0));
  float inset = step(0.1, f.x) * step(f.x, 0.9) * step(0.1, f.y) * step(f.y, 0.9);
  fragColor = vec4(vec3(on * inset), 1.0);
}
