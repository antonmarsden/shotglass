// Reaction-diffusion seed: A = 1 everywhere, B = 1 in a disc.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  float b = step(length(fragCoord - iResolution.xy * 0.5), 6.0);
  fragColor = vec4(1.0, b, 0.0, 1.0);
}
