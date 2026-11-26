// Reads glyph tables (format 1: the module doc of src/glyphrun.rs) and draws glyphs
// from their MTSDF atlas. Film-local, so the golden doesn't depend on the kit
// (shotglass-kit's text/runs.glsl does this for films). Lengths are layout pixels,
// y down from the block's top, as in the table.

uvec4 tableBytes(sampler2D t, int i) {
  return uvec4(texelFetch(t, ivec2(i % 1024, i / 1024), 0) * 255.0 + 0.5);
}
uint table32(sampler2D t, int i) {
  uvec4 b = tableBytes(t, i);
  return (b.r << 24) | (b.g << 16) | (b.b << 8) | b.a;
}
uvec2 table16(sampler2D t, int i) {
  uvec4 b = tableBytes(t, i);
  return uvec2((b.r << 8) | b.g, (b.b << 8) | b.a);
}
float signed16(uint v) { return float(int(v) - (v >= 32768u ? 65536 : 0)); }

struct Table {
  int glyphs, lines, firstLine, firstGlyph, lineTexels, glyphTexels;
  int entries, firstEntry, entryTexels;
  float em, atlasEm, spread, pitch;
};

Table readTable(sampler2D t) {
  Table h;
  h.glyphs = int(table32(t, 1));
  h.lines = int(table32(t, 2));
  h.firstLine = int(table32(t, 3));
  h.firstGlyph = int(table32(t, 4));
  uvec2 em = table16(t, 5);
  h.em = float(em.x) / 16.0;
  h.atlasEm = float(em.y) / 16.0;
  uvec2 sp = table16(t, 6);
  h.spread = float(sp.x) / 256.0;
  h.pitch = float(sp.y) / 16.0;
  uvec2 rec = table16(t, 7);
  h.lineTexels = int(rec.x);
  h.glyphTexels = int(rec.y);
  h.entries = int(table32(t, 8));
  h.firstEntry = int(table32(t, 9));
  h.entryTexels = int(table16(t, 10).x);
  return h;
}

struct Line {
  float baseline, x0, x1;
  int first, count;
};

Line readLine(sampler2D t, Table h, int i) {
  int at = h.firstLine + i * h.lineTexels;
  Line l;
  l.baseline = float(table32(t, at)) / 256.0;
  uvec2 range = table16(t, at + 1);
  l.first = int(range.x);
  l.count = int(range.y);
  uvec2 span = table16(t, at + 2);
  l.x0 = float(span.x) / 16.0;
  l.x1 = float(span.y) / 16.0;
  return l;
}

struct Glyph {
  vec2 corner;   // the tile's top-left corner, layout pixels
  vec2 size;     // the tile's size, layout pixels (0 for no ink)
  ivec2 tile;    // the tile's top-left texel in the atlas
  int grapheme, word, line;
  uint code;     // glyph sets: the code point
  float advance; // glyph sets: the advance
};

// Glyph i. For a run, `baseline` is its line's (readLine); for a set, the pen is at
// x 0 on baseline 0 and `advance`/`code` are filled in.
Glyph readGlyph(sampler2D t, Table h, int i, float baseline, bool isSet) {
  int at = h.firstGlyph + i * h.glyphTexels;
  float k = h.em / h.atlasEm;   // layout pixels per atlas texel
  uvec2 pen = table16(t, at);
  uvec2 pos = table16(t, at + 1);
  uvec2 wh = table16(t, at + 2);
  uvec2 lt = table16(t, at + 3);
  uvec2 tail = table16(t, at + 5);
  Glyph g;
  float x = float(pen.x) / 16.0 + signed16(tail.y) / 16.0;
  float y = baseline + signed16(pen.y) / 16.0;
  g.corner = vec2(x + signed16(lt.x) / 16.0 * k, y - signed16(lt.y) / 16.0 * k);
  g.size = vec2(wh) * k;
  g.tile = ivec2(pos);
  if (isSet) {
    g.code = table32(t, at + 4);
    g.advance = float(tail.x) / 16.0;
    g.grapheme = g.word = g.line = 0;
  } else {
    uvec2 gw = table16(t, at + 4);
    g.grapheme = int(gw.x);
    g.word = int(gw.y);
    g.line = int(tail.x);
    g.code = 0u;
    g.advance = 0.0;
  }
  return g;
}

float median3(vec3 v) { return max(min(v.r, v.g), min(max(v.r, v.g), v.b)); }

// The glyph's signed distance at p (layout pixels), in layout pixels, inside
// positive; very negative outside its tile. Bilinear in float from texelFetch:
// hardware filtering weights differ between drivers (llvmpipe's are 8-bit). `alpha`
// gets the true (single-channel) distance, for glows.
float glyphDistance(sampler2D atlas, Table h, Glyph g, vec2 p, out float alpha) {
  alpha = -1e3;
  if (g.size.x <= 0.0) return -1e3;
  vec2 q = p - g.corner;
  if (any(lessThan(q, vec2(0.0))) || any(greaterThan(q, g.size))) return -1e3;
  float k = h.em / h.atlasEm;
  vec2 a = q / k - 0.5;          // tile texel space, centres at integers
  vec2 i = floor(a);
  vec2 f = a - i;
  ivec2 hi = ivec2(g.size / k + 0.5) - 1;
  ivec2 c0 = g.tile + clamp(ivec2(i), ivec2(0), hi);
  ivec2 c1 = g.tile + clamp(ivec2(i) + 1, ivec2(0), hi);
  vec4 d00 = texelFetch(atlas, c0, 0);
  vec4 d10 = texelFetch(atlas, ivec2(c1.x, c0.y), 0);
  vec4 d01 = texelFetch(atlas, ivec2(c0.x, c1.y), 0);
  vec4 d11 = texelFetch(atlas, c1, 0);
  vec4 d = mix(mix(d00, d10, f.x), mix(d01, d11, f.x), f.y);
  // 0.5 is the outline (127.5/255); the field spans ±spread atlas texels.
  float toLayout = 2.0 * h.spread * k;
  alpha = (d.a - 127.5 / 255.0) * toLayout;
  return (median3(d.rgb) - 127.5 / 255.0) * toLayout;
}
