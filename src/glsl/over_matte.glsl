// The engine's matted layer composite (#74): the over pass with the layer (premultiplied
// by the layer footer) times its opacity (`progress`) and its matte (`sg_matte_value`,
// the matte source's alpha or luma, maybe inverted) over the picture below it.
// uFrom = below, uTo = layer. Not a transition films can name.
vec4 transition(vec2 uv) {
  vec4 l = getToColor(uv) * (progress * sg_matte_value(uv));
  vec4 b = getFromColor(uv);
  return vec4(l.rgb + b.rgb * (1.0 - l.a), 1.0);
}
