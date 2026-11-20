// Plain cross-dissolve. Renders both shots unless freeze = true.
vec4 transition(vec2 uv) {
    return mix(getFromColor(uv), getToColor(uv), progress);
}
