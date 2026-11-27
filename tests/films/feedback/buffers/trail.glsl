// The brighter of this frame's picture (iChannel1) and the decayed previous state.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec4 prev = texture(iChannel0, uv);
  vec4 pic = texture(iChannel1, uv);
  fragColor = max(pic, prev * uDecay);
}
