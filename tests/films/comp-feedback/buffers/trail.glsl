// The brighter of the comp's picture (iChannel1) and the decayed previous state
// (iChannel0); the bottom rows hold the buffer-local iFrame (frames since the last
// reset) as a blue bar, one texel column per frame.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  if (fragCoord.y < 3.0) {
    fragColor = vec4(0.0, 0.0, fragCoord.x < float(iFrame) + 1.0 ? 1.0 : 0.0, 1.0);
    return;
  }
  vec4 prev = texture(iChannel0, uv);
  vec4 pic = texture(iChannel1, uv);
  fragColor = max(pic, prev * uDecay);
}
