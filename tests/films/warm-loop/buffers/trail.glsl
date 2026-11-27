// The brighter of the element (iChannel1) and the decayed previous state (iChannel0).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  fragColor = max(texture(iChannel1, uv), texture(iChannel0, uv) * 0.9);
}
