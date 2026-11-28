// A warm grade of everything below (an opaque filter).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 c = texture(iChannel0, uv).rgb;
  fragColor = vec4(c * vec3(1.1, 1.0, 0.85) + vec3(0.02, 0.0, 0.0), 1.0);
}
