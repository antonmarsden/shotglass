// A disc moving left to right on transparent (straight alpha), a little faster than a
// panel's width per second; iTime is the layer's own (0 at 0.2 s).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 p = vec2(0.1 + 0.6 * iTime, 0.5 + 0.2 * sin(4.0 * iTime));
  vec2 d = (uv - p) * vec2(iResolution.x / iResolution.y, 1.0);
  float a = smoothstep(0.09, 0.07, length(d));
  fragColor = vec4(vec3(1.0, 0.85, 0.3), a);
}
