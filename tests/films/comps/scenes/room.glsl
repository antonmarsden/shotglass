void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 col = mix(vec3(0.25, 0.2, 0.15), vec3(0.5, 0.45, 0.4), uv.y);
  // The TV: iChannel0 is comp "tv" at its own 96×54, at this shot's iTime.
  vec2 lo = vec2(0.3, 0.3), hi = vec2(0.7, 0.75);
  if (all(greaterThan(uv, lo)) && all(lessThan(uv, hi))) {
    col = texture(iChannel0, (uv - lo) / (hi - lo)).rgb;
  }
  fragColor = vec4(col, 1.0);
}
