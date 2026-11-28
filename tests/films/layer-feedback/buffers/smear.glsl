// A temporal smear of the composite up to "grade" (iChannel1): keep uKeep of the
// previous state. The plate doesn't move, so only the disc smears.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec4 now = texture(iChannel1, uv);
  vec4 prev = texture(iChannel0, uv);
  fragColor = iFrame == 0 ? now : mix(now, prev, uKeep);
}
