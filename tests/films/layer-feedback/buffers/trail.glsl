// A trail of layer "title"'s own picture (iChannel1, premultiplied over transparent):
// the brighter of it and the decayed previous state. Before 0.2 s the layer is
// inactive: the channel is unbound (resolution 0, reads opaque black), so it adds
// nothing.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec4 title = iChannelResolution[1].x > 0.0 ? texture(iChannel1, uv) : vec4(0.0);
  fragColor = max(title, texture(iChannel0, uv) * uDecay);
}
