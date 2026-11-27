// The plate (iChannel0) with the warm trail (iChannel1) in orange and the late one
// (iChannel2) in blue, added (an inactive buffer reads black).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 c = texture(iChannel0, uv).rgb;
  float warm = texture(iChannel1, uv).r;
  float late = texture(iChannel2, uv).r;
  fragColor = vec4(c + vec3(1.0, 0.5, 0.1) * warm + vec3(0.1, 0.45, 1.0) * late, 1.0);
}
