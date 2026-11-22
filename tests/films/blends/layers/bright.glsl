// Bright pass: what's above a threshold in the picture below, else nothing. Drawn at
// opacity 0, so it adds nothing itself; "bloom" reads it with @glow.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec3 c = texture(iChannel0, fragCoord / iResolution.xy).rgb;
  float lum = dot(c, vec3(0.299, 0.587, 0.114));
  fragColor = vec4(c * smoothstep(0.5, 0.8, lum), 1.0);
}
