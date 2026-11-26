// Subtitles: the cue whose time covers iTime (the table's entry records), centred
// near the bottom on a box sized from its line records; and a clock (seconds, two
// decimals) top right, drawn character by character from a glyph set found by
// binary search on the code point. iChannel0/1 = subtitle atlas/table,
// iChannel2/3 = glyph set atlas/table. Straight alpha (the engine premultiplies).

#include "../scenes/glyphs.glsl"

// Ink of a set character at p (pen at x 0, baseline 0), and its advance.
float setChar(uint code, vec2 p, Table h, float s, out float advance) {
  int lo = 0, hi = h.glyphs - 1;
  advance = 0.0;
  while (lo <= hi) {
    int mid = (lo + hi) / 2;
    uint c = table32(iChannel3, h.firstGlyph + mid * h.glyphTexels + 4);
    if (c == code) {
      Glyph g = readGlyph(iChannel3, h, mid, 0.0, true);
      advance = g.advance;
      float alpha;
      return clamp(glyphDistance(iChannel2, h, g, p, alpha) * s + 0.5, 0.0, 1.0);
    }
    if (c < code) lo = mid + 1; else hi = mid - 1;
  }
  return 0.0;
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 res = iResolution.xy;
  float s = res.y / 720.0;
  vec4 outc = vec4(0.0);

  // Subtitles.
  Table h = readTable(iChannel1);
  uint ms = uint(iTime * 1000.0);
  for (int e = 0; e < h.entries; e++) {
    int at = h.firstEntry + e * h.entryTexels;
    uint start = table32(iChannel1, at), end = table32(iChannel1, at + 1);
    if (ms < start || ms >= end) continue;
    uvec2 lines = table16(iChannel1, at + 2);
    Line top = readLine(iChannel1, h, int(lines.x));
    Line bottom = readLine(iChannel1, h, int(lines.x + lines.y) - 1);
    // The cue's last baseline sits 80 layout pixels above the bottom.
    vec2 origin = vec2(0.5 * (top.x0 + top.x1), bottom.baseline);
    vec2 p = (fragCoord - vec2(0.5 * res.x, 80.0 * s)) / s * vec2(1.0, -1.0) + origin;
    float x0 = 1e9, x1 = -1e9;
    float ink = 0.0;
    for (uint li = lines.x; li < lines.x + lines.y; li++) {
      Line l = readLine(iChannel1, h, int(li));
      x0 = min(x0, l.x0);
      x1 = max(x1, l.x1);
      for (int gi = l.first; gi < l.first + l.count; gi++) {
        Glyph g = readGlyph(iChannel1, h, gi, l.baseline, false);
        float alpha;
        ink = max(ink, clamp(glyphDistance(iChannel0, h, g, p, alpha) * s + 0.5, 0.0, 1.0));
      }
    }
    vec2 boxLo = vec2(x0 - 16.0, top.baseline - h.em * 0.95);
    vec2 boxHi = vec2(x1 + 16.0, bottom.baseline + h.em * 0.35);
    float box = (all(greaterThan(p, boxLo)) && all(lessThan(p, boxHi))) ? 0.6 : 0.0;
    outc = vec4(0.0, 0.0, 0.0, box);
    outc = mix(outc, vec4(1.0), ink);
  }

  // Clock: "t.tts", right aligned, 40 layout pixels in from the top right.
  Table c = readTable(iChannel3);
  int hund = int(iTime * 100.0 + 0.5);
  uint text[5] = uint[5](uint(48 + hund / 100 % 10), 46u, uint(48 + hund / 10 % 10),
                         uint(48 + hund % 10), 115u);
  vec2 q = (vec2(res.x, res.y) - fragCoord) / s;   // from the top right, y down, x left
  float pen = 0.0;
  float total = 0.0;
  for (int i = 0; i < 5; i++) {
    float adv;
    setChar(text[i], vec2(-1e4), c, s, adv);
    total += adv;
  }
  vec2 p = vec2(total - q.x + 40.0, q.y - 40.0 - c.em * 0.75);
  float ink = 0.0;
  for (int i = 0; i < 5; i++) {
    float adv;
    ink = max(ink, setChar(text[i], p - vec2(pen, 0.0), c, s, adv));
    pen += adv;
  }
  outc = mix(outc, vec4(0.95, 0.85, 0.5, 1.0), ink);
  fragColor = outc;
}
