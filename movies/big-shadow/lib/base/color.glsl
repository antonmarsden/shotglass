// Big Shadow, lib/base: color.glsl
#include "noise.glsl"

vec3 bs_aces(vec3 c) {
    c = max(c, 0.0);
    return clamp((c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14), 0.0, 1.0);
}

vec3 bs_grain(vec3 col, vec2 fragCoord, int frame, float amount) {
    float g = bs_hash(ivec3(ivec2(floor(fragCoord)), frame)) - 0.5;
    return col + g * amount;
}
