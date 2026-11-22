// The engine's layer composite (#69): the layer (premultiplied by the layer footer) at
// its opacity (`progress`) over the picture below it. uFrom = below, uTo = layer.
// Not a transition films can name; blend modes other than "over" come from libraries.
vec4 transition(vec2 uv) {
  vec4 l = getToColor(uv) * progress;
  vec4 b = getFromColor(uv);
  return vec4(l.rgb + b.rgb * (1.0 - l.a), 1.0);
}
