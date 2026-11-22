// Reads the picture below (@input on iChannel2) and darkens and warms its edges; opaque,
// so at opacity o it's an o-strength grade.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iChannelResolution[2].xy;
  vec3 below = texture(iChannel2, uv).rgb;
  float v = smoothstep(0.85, 0.25, length(uv - 0.5));
  fragColor = vec4(below * mix(vec3(0.25, 0.2, 0.15), vec3(1.0), v), 1.0);
}
