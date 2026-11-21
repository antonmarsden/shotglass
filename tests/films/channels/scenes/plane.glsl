// A floor receding to the horizon (mipmapped, repeating grid) under a sky (the photo).
// Both textures are sampled outside any branch, so derivatives are well defined.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 p = (2.0 * fragCoord - iResolution.xy) / iResolution.y;
  float below = -min(p.y, -0.02);
  float z = 1.0 / below;
  vec2 floorUv = vec2(p.x * z, z + iTime * 0.75) * 0.35;
  vec3 floorCol = texture(iChannel0, floorUv).rgb * exp(-0.04 * z);
  vec2 uv = fragCoord / iResolution.xy;
  vec3 sky = texture(iChannel1, vec2(uv.x, clamp((uv.y - 0.5) * 2.0, 0.0, 1.0))).rgb;
  fragColor = vec4(mix(sky, floorCol, step(p.y, 0.0)), 1.0);
}
