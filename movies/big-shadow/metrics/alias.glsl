// Spatial aliasing: how far each pixel is from the supersampled reference
// (RGB distance, 0 to sqrt(3)). Stand-in until shotglass-kit ships quality/alias.
void mainMetric(out vec4 value, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float d = length(getFrame(uv).rgb - getReference(uv).rgb);
  value = vec4(vec3(d * GAIN), d);
}
