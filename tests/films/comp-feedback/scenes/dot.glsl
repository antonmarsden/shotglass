// The comp's picture: an orange dot crossing the frame over black (comp-local time).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = fragCoord / iResolution.y;
  vec2 c = vec2(0.2 + 1.3 * fract(iTime * 0.6), 0.5 + 0.25 * sin(iTime * 6.0));
  float a = smoothstep(0.1, 0.08, length(p - c));
  fragColor = vec4(vec3(1.0, 0.6, 0.2) * a, 1.0);
}
