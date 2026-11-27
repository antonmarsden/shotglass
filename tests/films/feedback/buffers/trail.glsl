// A trail of the element (iChannel1, premultiplied over transparent): the brighter of
// it and the decayed previous state, so the trail stays transparent where nothing passed.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec4 prev = texture(iChannel0, uv);
  vec4 dot = texture(iChannel1, uv);
  fragColor = max(dot, prev * uDecay);
}
