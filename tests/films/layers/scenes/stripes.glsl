void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float s = step(0.5, fract(uv.x * 8.0 + uv.y * 2.0 + iTime * 0.5));
  fragColor = vec4(mix(vec3(0.1, 0.5, 0.3), vec3(0.9, 0.9, 0.8), s), 1.0);
}
