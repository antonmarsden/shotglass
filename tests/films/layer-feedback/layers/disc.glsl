// The title layer, on transparent (straight alpha): a disc moving left to right and a
// thin cyan frame around the middle of the picture. iTime is the layer's own (0 at
// 0.2 s). It's below the tunnel buffer, so both fall into the feedback.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec2 asp = vec2(iResolution.x / iResolution.y, 1.0);
  vec2 p = vec2(0.1 + 0.6 * iTime, 0.5 + 0.2 * sin(4.0 * iTime));
  float disc = smoothstep(0.09, 0.07, length((uv - p) * asp));
  vec2 d = abs(uv - 0.5) * asp;
  float box = max(d.x, d.y);
  float frame = 1.0 - smoothstep(0.004, 0.008, abs(box - 0.3));
  vec3 c = mix(vec3(0.3, 0.9, 1.0), vec3(1.0, 0.85, 0.3), disc);
  fragColor = vec4(c, max(disc, frame));
}
