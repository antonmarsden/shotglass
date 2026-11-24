// Big Shadow, lib/sound: drum.glsl
#include "envelope.glsl"

uint bs_drum_hash(uint x) {
    x ^= x >> 16;
    x *= 0x7feb352dU;
    x ^= x >> 15;
    x *= 0x846ca68bU;
    x ^= x >> 16;
    return x;
}

float bs_drum_white(int i, uint seed) {
    return float(bs_drum_hash(uint(i) ^ seed) >> 8) * (2.0 / 16777216.0) - 1.0;
}

float bs_drum_noise(float t, uint seed, bool high) {
    float x = clamp(t, 0.0, 60.0) * 24000.0;
    float fi = floor(x);
    int i = int(fi);
    float a = bs_drum_white(i, seed), b = bs_drum_white(i + 1, seed);
    if (high) {
        a = 0.5 * (a - bs_drum_white(i - 1, seed));
        b = 0.5 * (b - bs_drum_white(i, seed));
    }
    return mix(a, b, x - fi);
}

float bs_drum_hit(float t, float attack, float decay) {
    return smoothstep(0.0, attack, t) * bs_env_ar(t, 0.0, decay);
}

float bs_drum_kick(float t, float hz, float decay) {
    if (t < 0.0 || t >= decay) return 0.0;
    float e = bs_drum_hit(t, 0.002, decay);
    float tau = 0.03;
    float phase = hz * t + 2.0 * hz * tau * (1.0 - exp(-t / tau));
    float body = sin(6.28318531 * fract(phase));
    float ce = bs_drum_hit(t, 0.001, 0.006);
    float click = bs_drum_noise(t, 0x2545f491U, false);
    return e * (0.85 * body) + 0.15 * ce * click;
}
