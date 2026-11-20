// Noisy dissolve: blotches of the new shot eat through the old one.
#ifndef scale
#define scale 9.0
#endif
#ifndef softness
#define softness 0.08
#endif
float sg_h(vec2 p) { return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453); }
float sg_n(vec2 p) {
    vec2 i = floor(p), f = fract(p); f = f * f * (3.0 - 2.0 * f);
    return mix(mix(sg_h(i), sg_h(i + vec2(1, 0)), f.x), mix(sg_h(i + vec2(0, 1)), sg_h(i + vec2(1, 1)), f.x), f.y);
}
vec4 transition(vec2 uv) {
    vec2 p = uv * vec2(ratio, 1.0) * scale;
    float n = 0.6 * sg_n(p) + 0.3 * sg_n(p * 2.1) + 0.1 * sg_n(p * 4.3);
    float t = mix(-softness, 1.0 + softness, progress);
    float m = smoothstep(t - softness, t + softness, n);
    return mix(getToColor(uv), getFromColor(uv), m);
}
