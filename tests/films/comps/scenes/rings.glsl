void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (fragCoord - 0.5 * iResolution.xy) / iResolution.y;
  float r = length(p);
  float k = 0.5 + 0.5 * cos(40.0 * r - 6.0 * iTime);
  fragColor = vec4(mix(vec3(0.1, 0.2, 0.5), vec3(0.9, 0.8, 0.3), k), 1.0);
}
