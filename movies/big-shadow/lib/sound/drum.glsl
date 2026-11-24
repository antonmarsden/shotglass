// Big Shadow, lib/sound: drum.glsl
#include "envelope.glsl"

uint kit__drum_hash(uint x) {
    x ^= x >> 16;
    x *= 0x7feb352dU;
    x ^= x >> 15;
    x *= 0x846ca68bU;
    x ^= x >> 16;
    return x;
}

float kit__drum_white(int i, uint seed) {
    return float(kit__drum_hash(uint(i) ^ seed) >> 8) * (2.0 / 16777216.0) - 1.0;
}

float kit__drum_noise(float t, uint seed, bool high) {
    float x = clamp(t, 0.0, 60.0) * 24000.0;
    float fi = floor(x);
    int i = int(fi);
    float a = kit__drum_white(i, seed), b = kit__drum_white(i + 1, seed);
    if (high) {
        a = 0.5 * (a - kit__drum_white(i - 1, seed));
        b = 0.5 * (b - kit__drum_white(i, seed));
    }
    return mix(a, b, x - fi);
}

float kit__drum_hit(float t, float attack, float decay) {
    return smoothstep(0.0, attack, t) * kit_env_ar(t, 0.0, decay);
}

float kit_drum_kick(float t, float hz, float decay) {
    if (t < 0.0 || t >= decay) return 0.0;
    float e = kit__drum_hit(t, 0.002, decay);
    float tau = 0.03;
    float phase = hz * t + 2.0 * hz * tau * (1.0 - exp(-t / tau));
    float body = sin(6.28318531 * fract(phase));
    float ce = kit__drum_hit(t, 0.001, 0.006);
    float click = kit__drum_noise(t, 0x2545f491U, false);
    return e * (0.85 * body) + 0.15 * ce * click;
}
