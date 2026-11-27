// Four panels: the trail (top left), an echo of the picture history (top right: now,
// 2, 4 and 8 frames ago), reaction-diffusion (bottom left: B red, A green, iFrame
// blue) and the history's current state (bottom right).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 q = vec2(fract(uv.x * 2.0), fract(uv.y * 2.0));
  vec3 c;
  if (uv.y >= 0.5) {
    if (uv.x < 0.5) {
      c = texture(iChannel0, q).rgb;
    } else {
      c = texture(iChannel1, q).rgb + 0.6 * texture(iChannel2, q).rgb +
          0.4 * texture(iChannel3, q).rgb + 0.25 * texture(iChannel4, q).rgb;
    }
  } else if (uv.x < 0.5) {
    vec4 r = texture(iChannel5, q);
    c = vec3(r.y, 0.4 * r.x, r.z);
  } else {
    c = texture(iChannel1, q).rgb;
  }
  fragColor = vec4(min(c, vec3(1.0)), 1.0);
}
