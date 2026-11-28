// A crisp white frame inset from the edges and a centre cross, on transparent. It's
// above the tunnel buffer, so it stays sharp and never falls into the feedback.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 px = fragCoord;
  vec2 res = iResolution.xy;
  float inset = floor(res.y * 0.08);
  float t = max(1.0, floor(res.y / 180.0));
  vec2 lo = px - inset;
  vec2 hi = res - inset - px;
  bool inFrame = min(lo.x, lo.y) >= 0.0 && min(hi.x, hi.y) >= 0.0;
  bool edge = inFrame && (min(lo.x, hi.x) < t || min(lo.y, hi.y) < t);
  vec2 d = abs(px - res * 0.5);
  bool cross = (d.x < t * 0.5 + 0.5 && d.y < inset * 0.5) || (d.y < t * 0.5 + 0.5 && d.x < inset * 0.5);
  fragColor = vec4(1.0, 1.0, 1.0, (edge || cross) ? 1.0 : 0.0);
}
