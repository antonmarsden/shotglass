// A dot crossing the frame on a dark ground.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = fragCoord / iResolution.y;
  vec2 c = vec2(0.2 + 1.2 * fract(iTime * 0.7), 0.5 + 0.25 * sin(iTime * 5.0));
  float d = smoothstep(0.09, 0.07, length(p - c));
  fragColor = vec4(vec3(0.05) + uColor * d, 1.0);
}
