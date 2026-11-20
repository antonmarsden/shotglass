// Classic cartoon iris. Opening (default): the new shot appears in a growing circle.
// closing = 1.0: the old shot shrinks into a circle and the new shot is outside it.
#ifndef center
#define center vec2(0.5, 0.5)
#endif
#ifndef softness
#define softness 0.004
#endif
#ifndef closing
#define closing 0.0
#endif
vec4 transition(vec2 uv) {
    vec2 a = vec2(ratio, 1.0);
    vec2 c = center * a;
    // radius that just covers the farthest corner
    float far = max(max(length(c), length(c - vec2(a.x, 0.0))),
                    max(length(c - vec2(0.0, 1.0)), length(c - a)));
    float d = length(uv * a - c);
    if (closing > 0.5) {
        float r = (1.0 - progress) * far;
        float inside = 1.0 - smoothstep(r - softness, r + softness, d);
        return mix(getToColor(uv), getFromColor(uv), inside);
    }
    float r = progress * far;
    float inside = 1.0 - smoothstep(r - softness, r + softness, d);
    return mix(getFromColor(uv), getToColor(uv), inside);
}
