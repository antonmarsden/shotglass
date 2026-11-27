// The animated element on a transparent background, premultiplied: a dot crossing the
// frame, orange in shot "a", blue (and half a second further on) in shot "b". It reads
// no buffer state, so it's a pure history buffer.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  bool b = iMovieTime >= 1.0;
  float t = b ? iMovieTime - 0.5 : iMovieTime;
  vec3 color = b ? vec3(0.3, 0.7, 1.0) : vec3(1.0, 0.6, 0.2);
  vec2 p = fragCoord / iResolution.y;
  vec2 c = vec2(0.2 + 1.2 * fract(t * 0.7), 0.5 + 0.25 * sin(t * 5.0));
  float a = smoothstep(0.09, 0.07, length(p - c));
  fragColor = vec4(color * a, a);
}
