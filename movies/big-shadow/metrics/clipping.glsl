// Clipped highlights: 1 where any channel is at full scale, else 0, so the mean is
// the clipped share of the frame.
void mainMetric(out vec4 value, in vec2 fragCoord) {
  vec3 c = getFrame(fragCoord / iResolution.xy).rgb;
  float clipped = step(LEVEL, max(c.r, max(c.g, c.b)));
  value = vec4(clipped, 0.0, 0.0, clipped);
}
