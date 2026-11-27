// The background plate: a dark vertical gradient, tinted per shot. The moving element
// is drawn into its own buffer (buffers/dot.glsl), as a film would keep an animated
// element apart from its background to echo or trail it.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 ground = mix(vec3(0.02), vec3(0.09), uv.y) + 0.04 * uTint;
  fragColor = vec4(ground, 1.0);
}
