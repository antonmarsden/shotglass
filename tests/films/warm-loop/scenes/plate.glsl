// A dark, still plate.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  fragColor = vec4(vec3(0.05, 0.05, 0.07) + 0.04 * uv.y, 1.0);
}
