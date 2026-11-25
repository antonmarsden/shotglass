void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float x = iTime / COMP_DURATION;
  float bar = smoothstep(0.08, 0.0, abs(uv.x - x));
  fragColor = vec4(vec3(0.1, 0.5, 0.2) + bar * vec3(0.9, 0.5, 0.8) * uv.y, 1.0);
}
