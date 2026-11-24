// Big Shadow, lib/transitions: fade
#ifndef color
#define color vec3(0.0)
#endif
#ifndef hold
#define hold 0.0
#endif
vec4 transition(vec2 uv) {
    float h = clamp(hold, 0.0, 0.9);
    float a = 0.5 - 0.5 * h, b = 0.5 + 0.5 * h;
    if (progress < a) return vec4(mix(getFromColor(uv).rgb, color, progress / a), 1.0);
    if (progress < b) return vec4(color, 1.0);
    return vec4(mix(color, getToColor(uv).rgb, (progress - b) / (1.0 - b)), 1.0);
}
