// Four panels: nearest + clamp (zoomed out: the edge texels streak), linear + mirror
// with vflip = false (upside down), nearest zoomed in, and the photo repeating with an
// explicit gradient (the panel seams would otherwise upset the mip choice). A bar under
// each panel is iChannelResolution[n].x / 128 of its width: half for the 64-px images,
// nothing for the unbound iChannel3 (which samples black).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float panel = floor(uv.x * 4.0);
  vec2 q = vec2(fract(uv.x * 4.0), uv.y);
  vec2 c = (q - 0.5) * vec2(iResolution.x / 4.0 / iResolution.y, 1.0);
  vec2 g = vec2(1.0 / iResolution.y, 0.0);
  vec3 a = texture(iChannel0, c * 1.6 + 0.5).rgb;
  vec3 b = texture(iChannel1, c * 2.5 + 0.5 + vec2(iShotTime * 0.2, 0.0)).rgb;
  vec3 n = texture(iChannel0, c * 0.4 + 0.5).rgb;
  vec3 d = textureGrad(iChannel2, c * 2.0 + 0.5 + iShotTime * 0.1, g.xy * 2.0, g.yx * 2.0).rgb;
  vec3 e = texture(iChannel3, c).rgb;
  vec3 col = panel < 1.0 ? a : panel < 2.0 ? b : panel < 3.0 ? n : d + e;
  int i = int(panel);
  float res = i == 0 ? iChannelResolution[0].x : i == 1 ? iChannelResolution[1].x
            : i == 2 ? iChannelResolution[0].x : iChannelResolution[3].x;
  if (uv.y < 0.04) {
    col = q.x < res / 128.0 ? vec3(1.0) : vec3(0.1);
  }
  fragColor = vec4(col, 1.0);
}
