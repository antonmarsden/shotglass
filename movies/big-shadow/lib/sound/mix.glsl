// Big Shadow, lib/sound: mix.glsl
#include "master.glsl"

const int bs_bus_music = 0;   // music(samp): the music track
const int bs_bus_sfx = 1;     // sfx(samp): every shot sound, faded for transitions
const int bs_bus_all = 2;     // music(samp) + sfx(samp)

vec2 bs_mix_read(int bus, int samp) {
    if (bus == bs_bus_music) return music(samp);
    if (bus == bs_bus_sfx) return sfx(samp);
    return music(samp) + sfx(samp);
}

int bs_mix_samples(float seconds, int rate) {
    return int(floor(max(seconds, 0.0) * float(rate) + 0.5));
}

vec2 bs_mix_delay(int bus, int samp, int rate, float time, int taps, float feedback,
                   float pingpong) {
    int d = max(bs_mix_samples(time, rate), 1);
    int n = clamp(taps, 1, 16);
    float fb = clamp(feedback, 0.0, 0.99);
    float pp = clamp(pingpong, 0.0, 1.0);
    vec2 o = vec2(0.0);
    float g = 1.0;
    for (int k = 1; k <= 16; k++) {
        if (k > n) break;
        vec2 v = bs_mix_read(bus, samp - k * d);
        if (k % 2 == 1) v = mix(v, v.yx, pp);
        o += v * g;
        g *= fb;
    }
    return o;
}

vec2 bs_mix_reverb(int bus, int samp, int rate, float size, float decay) {
    const float at[24] = float[24](
        0.1373, 0.2082, 0.2932, 0.3257, 0.3913, 0.4312, 0.4647, 0.5175,
        0.5431, 0.5897, 0.6159, 0.6503, 0.6905, 0.7308, 0.7463, 0.7783,
        0.8157, 0.8502, 0.8707, 0.8944, 0.9318, 0.9409, 0.9812, 1.0);
    float len = max(size, 0.01) * float(rate);
    float dec = clamp(decay, 0.001, 1.0);
    float dull = 0.00025 * float(rate) / 23.0;
    vec2 o = vec2(0.0);
    float energy = 0.0;
    for (int k = 0; k < 24; k++) {
        float g = pow(dec, at[k]);
        int d = int(floor(at[k] * len + 0.5));
        int w = int(floor(dull * float(k) + 0.5));
        vec2 v = 0.5 * (bs_mix_read(bus, samp - d) + bs_mix_read(bus, samp - d - w));
        v = mix(v, v.yx, fract(float(k) * 0.618034));
        o += v * g;
        energy += g * g;
    }
    return o * inversesqrt(energy);
}

float bs_mix_level(int bus, int samp, int rate, float attack, float release) {
    int a = bs_mix_samples(clamp(attack, 0.0, 0.05), rate);
    int r = bs_mix_samples(release, rate);
    int step = max((a + r + 62) / 63, 1);   // 64 points cover a + r
    int first = ((samp + a) / step) * step; // the furthest grid point ahead
    float fa = float(max(a, 1)), fr = float(max(r, 1));
    float m = 0.0;
    for (int j = 0; j < 64; j++) {
        int t = first - j * step;
        int d = t - samp;
        float w = d > 0 ? 1.0 - float(d) / fa : 1.0 + float(d) / fr;
        if (w <= 0.0) continue;
        vec2 v = abs(bs_mix_read(bus, t));
        m = max(m, max(v.x, v.y) * w);
    }
    return m;
}

float bs_mix_duck(int samp, int rate, float lo, float hi, float depth) {
    return bs_duck(bs_mix_level(bs_bus_sfx, samp, rate, 0.01, 0.15), lo, hi, depth);
}
