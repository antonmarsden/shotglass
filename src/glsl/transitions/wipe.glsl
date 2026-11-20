// Straight wipe. direction points the way the edge travels.
#ifndef direction
#define direction vec2(1.0, 0.0)
#endif
#ifndef softness
#define softness 0.01
#endif
vec4 transition(vec2 uv) {
    vec2 d = normalize(direction);
    // project the corners so the edge starts and ends just off screen
    float lo = min(min(dot(vec2(0, 0), d), dot(vec2(1, 0), d)), min(dot(vec2(0, 1), d), dot(vec2(1, 1), d)));
    float hi = max(max(dot(vec2(0, 0), d), dot(vec2(1, 0), d)), max(dot(vec2(0, 1), d), dot(vec2(1, 1), d)));
    float edge = mix(lo - softness, hi + softness, progress);
    float m = smoothstep(edge - softness, edge + softness, dot(uv, d));
    return mix(getToColor(uv), getFromColor(uv), m);
}
