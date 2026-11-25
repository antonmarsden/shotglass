void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  fragColor = vec4(mix(vec3(0.95, 0.6, 0.3), vec3(0.2, 0.4, 0.9), uv.y), 1.0);
}
