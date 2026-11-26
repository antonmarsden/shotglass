// A two-line glyph run: each glyph rises and fades in on its own, one grapheme after
// another (the table's grapheme index), tinted by its word, with a soft glow from the
// atlas's true distance (alpha). iChannel0 = the atlas, iChannel1 = the table.

#include "glyphs.glsl"

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 res = iResolution.xy;
  vec3 col = mix(vec3(0.04, 0.05, 0.09), vec3(0.1, 0.08, 0.15), fragCoord.y / res.y);
  Table h = readTable(iChannel1);
  Line first = readLine(iChannel1, h, 0);
  Line last = readLine(iChannel1, h, h.lines - 1);
  // Layout pixels are 1/720 of the height; the block is centred.
  float s = res.y / 720.0;
  vec2 centre = vec2(0.5 * (first.x0 + first.x1),
                     0.5 * (first.baseline - 0.75 * h.em + last.baseline + 0.25 * h.em));
  vec2 p = (fragCoord - 0.5 * res) / s * vec2(1.0, -1.0) + centre;
  vec3 tints[3] = vec3[3](vec3(1.0, 0.86, 0.45), vec3(0.55, 0.85, 1.0), vec3(1.0, 0.55, 0.65));
  for (int li = 0; li < h.lines; li++) {
    Line l = readLine(iChannel1, h, li);
    for (int gi = l.first; gi < l.first + l.count; gi++) {
      Glyph g = readGlyph(iChannel1, h, gi, l.baseline, false);
      float delay = 0.09 * float(g.grapheme);
      float a = smoothstep(delay, delay + 0.6, iTime);
      vec2 q = p - vec2(0.0, 70.0 * (1.0 - a));   // rising from below
      float alpha;
      float d = glyphDistance(iChannel0, h, g, q, alpha);
      // The true distance saturates at the tile edge (spread atlas texels): fade
      // the glow out by then, or it shows as a box.
      float reach = h.spread * h.em / h.atlasEm;
      float glow = (1.0 - smoothstep(0.0, reach, -alpha)) * 0.45;
      float ink = clamp(d * s + 0.5, 0.0, 1.0);
      vec3 tint = tints[g.word % 3];
      col += tint * glow * a;
      col = mix(col, tint, ink * a);
    }
  }
  fragColor = vec4(col, 1.0);
}
