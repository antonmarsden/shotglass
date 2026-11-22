// A dot grid with alpha: only the dots cover; drawn at half resolution (scale = 0.5).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 cell = fract(fragCoord / iResolution.y * 12.0) - 0.5;
  float a = 1.0 - smoothstep(0.18, 0.22, length(cell));
  fragColor = vec4(1.0, 1.0, 1.0, a);
}
