// Big Shadow, lib/kit: color.glsl
//
// Getting from "light" to pixels: tonemapping and grain.
// sound: yes
#include "noise.glsl"

// kit_aces(c): squeeze any brightness into 0..1 with a filmic curve, so highlights
// roll off instead of clipping (Narkowicz's ACES fit).
//   c   linear colour, 0..anything
//   ->  linear colour, 0..1
//   e.g. col = kit_aces(col * exposure);
vec3 kit_aces(vec3 c) {
    c = max(c, 0.0);
    return clamp((c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14), 0.0, 1.0);
}

// kit_grain(col, fragCoord, frame, amount): film grain, different every frame.
// It's deterministic (frame n always gets the same grain), so stills and exports match.
//   amount  0..0.2 typically; the grain's full range, in colour units
//   e.g.    col = kit_grain(col, fragCoord, iFrame, 0.04);
vec3 kit_grain(vec3 col, vec2 fragCoord, int frame, float amount) {
    float g = kit_hash(ivec3(ivec2(floor(fragCoord)), frame)) - 0.5;
    return col + g * amount;
}
