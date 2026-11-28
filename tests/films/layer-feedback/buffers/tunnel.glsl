// Video feedback of the composite up to "grade" (iChannel1): the previous state
// (iChannel0), shrunk 6% towards the centre and turned 0.03 rad each frame, decayed
// (dim echoes fade out at once, so the plate's grid doesn't smear), and the brighter
// of that and the picture now. The title's disc and frame spiral into the distance.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 asp = vec2(iResolution.x / iResolution.y, 1.0);
  // cos and sin of 0.03 as constants: built-in sin/cos differ between drivers, and a
  // feedback loop magnifies the difference.
  mat2 turn = mat2(0.99955003, 0.0299955, -0.0299955, 0.99955003);
  vec2 p = 0.5 + (turn * ((uv - 0.5) * asp) * 1.06) / asp;
  bool inside = all(greaterThanEqual(p, vec2(0.0))) && all(lessThanEqual(p, vec2(1.0)));
  vec4 prev = inside ? max(texture(iChannel0, p) * uDecay - 0.03, 0.0) : vec4(0.0);
  fragColor = max(texture(iChannel1, uv), prev);
}
