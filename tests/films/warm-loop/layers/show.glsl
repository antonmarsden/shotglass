// Two panels, so the trails never overlap: the warm trail (iChannel1, orange) squashed
// into the top half, the late one (iChannel2, blue; an inactive buffer reads black)
// into the bottom half, over the plate (iChannel0), with a thin line between.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 c = texture(iChannel0, uv).rgb;
  bool top = uv.y >= 0.5;
  vec2 st = vec2(uv.x, top ? uv.y * 2.0 - 1.0 : uv.y * 2.0);
  float trail = top ? texture(iChannel1, st).r : texture(iChannel2, st).r;
  vec3 col = top ? vec3(1.0, 0.5, 0.1) : vec3(0.1, 0.45, 1.0);
  float line = 1.0 - smoothstep(0.0, 1.5, abs(fragCoord.y - 0.5 * iResolution.y));
  fragColor = vec4(c + col * trail + vec3(0.25) * line, 1.0);
}
