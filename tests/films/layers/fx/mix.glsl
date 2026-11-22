// A plain crossfade, in the film so the golden test needs no library.
vec4 transition(vec2 uv) {
  return mix(getFromColor(uv), getToColor(uv), progress);
}
