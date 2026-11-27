// Four panels, each over the same background plate (@input, iChannel0): the trail (top
// left), an echo of the element's history (top right: 8, 4 and 2 frames ago, then now,
// each premultiplied over the last at less strength), reaction-diffusion (bottom left: B
// red, A green, iFrame blue) and the element now (bottom right). The element and its
// trails are transparent, so every panel shows the plate at the same brightness.
vec3 over(vec3 under, vec4 src, float w) {
  return under * (1.0 - src.a * w) + src.rgb * w;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 q = vec2(fract(uv.x * 2.0), fract(uv.y * 2.0));
  vec3 plate = texture(iChannel0, q).rgb;
  vec3 c;
  if (uv.y >= 0.5) {
    if (uv.x < 0.5) {
      c = over(plate, texture(iChannel1, q), 1.0);
    } else {
      c = over(plate, texture(iChannel5, q), 0.25);
      c = over(c, texture(iChannel4, q), 0.4);
      c = over(c, texture(iChannel3, q), 0.6);
      c = over(c, texture(iChannel2, q), 1.0);
    }
  } else if (uv.x < 0.5) {
    // It starts with shot "b"; before that the channel is unbound (resolution 0) and
    // the panel shows the plate.
    vec4 r = texture(iChannel6, q);
    c = iChannelResolution[6].x > 0.0 ? vec3(r.y, 0.4 * r.x, r.z) : plate;
  } else {
    c = over(plate, texture(iChannel2, q), 1.0);
  }
  fragColor = vec4(c, 1.0);
}
