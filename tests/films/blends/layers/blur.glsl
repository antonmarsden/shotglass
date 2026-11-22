// A wide blur of @glow (iChannel0, half resolution), screen-blended over the picture.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 px = 1.0 / iChannelResolution[0].xy;
  vec3 sum = vec3(0.0);
  float wsum = 0.0;
  for (int y = -4; y <= 4; y++) {
    for (int x = -4; x <= 4; x++) {
      float w = exp(-float(x * x + y * y) / 8.0);
      sum += w * texture(iChannel0, uv + 1.5 * px * vec2(float(x), float(y))).rgb;
      wsum += w;
    }
  }
  fragColor = vec4(2.2 * sum / wsum, 1.0);
}
