// Top: the title's coverage (red/alpha), fading in with uIn. Its baseline, from
// iChannelMeta[0].y, is a thin line. Bottom: a 4× zoom into the middle of the title
// drawn from the distance field (green): crisp edges at any scale, an outline band
// and a soft glow, all from the one texture. (uIn is declared from movie.toml.)

vec2 blockUV(vec2 p, vec2 centre, float h) {
  // p, centre in pixels; the block is drawn h pixels tall, aspect kept.
  vec2 size = iChannelResolution[0].xy;
  vec2 q = (p - centre) / h * vec2(size.y / size.x, 1.0) + 0.5;
  return q;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 res = iResolution.xy;
  vec3 col = mix(vec3(0.05, 0.06, 0.1), vec3(0.12, 0.1, 0.16), fragCoord.y / res.y);
  vec2 size = iChannelResolution[0].xy;
  vec4 meta = iChannelMeta[0];
  // Top half: the block 0.36 of the height tall.
  float h = res.y * 0.36;
  vec2 uv = blockUV(fragCoord, vec2(res.x * 0.5, res.y * 0.75), h);
  if (all(greaterThanEqual(uv, vec2(0.0))) && all(lessThanEqual(uv, vec2(1.0)))) {
    float cov = texture(iChannel0, uv).a;
    col = mix(col, vec3(1.0, 0.95, 0.85), cov * uIn);
    // The baseline: meta.y texels from the block's top (vflip: v = 1 at the top).
    float baseV = 1.0 - meta.y / size.y;
    float px = 1.0 / h;
    if (abs(uv.y - baseV) < px * 0.75) col = mix(col, vec3(0.9, 0.3, 0.2), 0.8);
  }
  // Bottom half: zoom 4× into the middle of the block.
  vec2 zuv = blockUV(fragCoord, vec2(res.x * 0.5, res.y * 0.28), h * 4.0);
  zuv.x += 0.32;
  if (fragCoord.y < res.y * 0.5 && all(greaterThanEqual(zuv, vec2(0.0))) && all(lessThanEqual(zuv, vec2(1.0)))) {
    float d = texture(iChannel0, zuv).g;           // 0.5 at the outline
    // Edge width from the known scale, not fwidth(d): derivatives of a texture
    // fetch differ between drivers. d moves 0.5 per spread (meta.w) texels, and a
    // pixel here covers size.y / (4h) texels.
    float w = 0.75 * (0.5 / meta.w) * size.y / (h * 4.0);
    float fill = smoothstep(0.5 - w, 0.5 + w, d);
    float band = smoothstep(0.42 - w, 0.42 + w, d) - fill;   // outline outside the ink
    float glow = smoothstep(0.1, 0.42, d) * (1.0 - fill - band);
    col += vec3(0.2, 0.4, 0.9) * glow * 0.6;
    col = mix(col, vec3(0.95, 0.75, 0.2), band);
    col = mix(col, vec3(1.0), fill);
  }
  fragColor = vec4(col, 1.0);
}
