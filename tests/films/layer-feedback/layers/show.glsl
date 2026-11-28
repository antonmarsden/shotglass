// Two panels, each the whole frame squeezed: on the left the smear of the composite up
// to "grade" (iChannel1); on the right the picture below (iChannel0) with the title's
// trail (iChannel2) over it in blue.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 q = vec2(fract(uv.x * 2.0), uv.y);
  vec3 c;
  if (uv.x < 0.5) {
    c = texture(iChannel1, q).rgb;
  } else {
    vec4 t = texture(iChannel2, q);
    c = texture(iChannel0, q).rgb * (1.0 - 0.6 * t.a) + 0.6 * t.a * vec3(0.3, 0.7, 1.0);
  }
  fragColor = vec4(c, 1.0);
}
