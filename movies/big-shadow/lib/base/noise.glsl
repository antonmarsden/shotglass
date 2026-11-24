// Big Shadow, lib/base: noise.glsl
uint bs_hash_u(uint x) {
    x ^= x >> 16;
    x *= 0x7feb352dU;
    x ^= x >> 15;
    x *= 0x846ca68bU;
    x ^= x >> 16;
    return x;
}
uint bs_hash_u(uvec2 v) { return bs_hash_u(v.x ^ bs_hash_u(v.y + 0x9e3779b9U)); }
uint bs_hash_u(uvec3 v) {
    return bs_hash_u(v.x ^ bs_hash_u(v.y ^ bs_hash_u(v.z + 0x9e3779b9U) + 0x7f4a7c15U));
}

float bs_unit(uint h) { return float(h >> 8) * (1.0 / 16777216.0); }

float bs_hash(int c) { return bs_unit(bs_hash_u(uint(c))); }
float bs_hash(ivec2 c) { return bs_unit(bs_hash_u(uvec2(c))); }
float bs_hash(ivec3 c) { return bs_unit(bs_hash_u(uvec3(c))); }

float bs_hash(vec2 p) { return bs_unit(bs_hash_u(floatBitsToUint(p))); }
float bs_hash(vec3 p) { return bs_unit(bs_hash_u(floatBitsToUint(p))); }

float bs_value_noise(vec2 p) {
    vec2 i = floor(p), f = fract(p);
    ivec2 c = ivec2(i);
    vec2 u = f * f * (3.0 - 2.0 * f);
    float a = bs_hash(c), b = bs_hash(c + ivec2(1, 0));
    float d = bs_hash(c + ivec2(0, 1)), e = bs_hash(c + ivec2(1, 1));
    return mix(mix(a, b, u.x), mix(d, e, u.x), u.y);
}
float bs_value_noise(vec3 p) {
    vec3 i = floor(p), f = fract(p);
    ivec3 c = ivec3(i);
    vec3 u = f * f * (3.0 - 2.0 * f);
    float n000 = bs_hash(c), n100 = bs_hash(c + ivec3(1, 0, 0));
    float n010 = bs_hash(c + ivec3(0, 1, 0)), n110 = bs_hash(c + ivec3(1, 1, 0));
    float n001 = bs_hash(c + ivec3(0, 0, 1)), n101 = bs_hash(c + ivec3(1, 0, 1));
    float n011 = bs_hash(c + ivec3(0, 1, 1)), n111 = bs_hash(c + ivec3(1, 1, 1));
    return mix(mix(mix(n000, n100, u.x), mix(n010, n110, u.x), u.y),
               mix(mix(n001, n101, u.x), mix(n011, n111, u.x), u.y), u.z);
}

float bs_fbm(vec2 p, int octaves) {
    const mat2 turn = mat2(0.8, 0.6, -0.6, 0.8);
    float sum = 0.0, amp = 0.5, total = 0.0;
    for (int o = 0; o < 12; o++) {
        if (o >= octaves) break;
        sum += amp * bs_value_noise(p);
        total += amp;
        p = turn * p * 2.0 + vec2(17.0, 31.0);
        amp *= 0.5;
    }
    return total > 0.0 ? sum / total : 0.0;
}
float bs_fbm(vec3 p, int octaves) {
    float sum = 0.0, amp = 0.5, total = 0.0;
    for (int o = 0; o < 12; o++) {
        if (o >= octaves) break;
        sum += amp * bs_value_noise(p);
        total += amp;
        p = p.yzx * 2.0 + vec3(17.0, 31.0, 47.0);
        amp *= 0.5;
    }
    return total > 0.0 ? sum / total : 0.0;
}
