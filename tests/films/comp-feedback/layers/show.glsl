// The comp's picture (iChannel0, @input) with its trail (iChannel1) added in green.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  vec3 pic = texture(iChannel0, uv).rgb;
  vec4 trail = texture(iChannel1, uv);
  fragColor = vec4(pic + vec3(0.0, 0.7, 0.3) * trail.r + vec3(0.0, 0.0, trail.b), 1.0);
}
