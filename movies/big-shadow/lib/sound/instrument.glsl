// Big Shadow, lib/sound: instrument.glsl
#include "envelope.glsl"

float kit__inst_sin(float turns) { return sin(6.28318531 * fract(turns)); }

float kit__inst_hit(float t, float attack, float decay) {
    return smoothstep(0.0, attack, t) * kit_env_ar(t, 0.0, decay);
}

float kit__inst_band(float hz) { return 1.0 - smoothstep(10000.0, 16000.0, hz); }

float kit_inst_pluck(float t, float hz, float decay, float bright) {
    if (t < 0.0 || t >= decay) return 0.0;
    bright = clamp(bright, 0.0, 1.0);
    float slope = 2.0 - 1.2 * bright;      // spectrum falls as 1/k^slope
    float damp = 1.5 - bright;             // how much faster each partial dies
    float s = 0.0, total = 0.0;
    for (int i = 1; i <= 12; i++) {
        float k = float(i);
        float f = k * hz * sqrt(1.0 + 0.00008 * k * k);   // slight inharmonicity
        float a = pow(k, -slope) * kit__inst_band(f);
        float e = kit__inst_hit(t, 0.003, decay / (1.0 + damp * (k - 1.0)));
        s += a * e * e * kit__inst_sin(f * t);
        total += a;
    }
    return s / max(total, 1e-6);
}

float kit_inst_bell(float t, float hz, float decay, float index) {
    if (t < 0.0 || t >= decay) return 0.0;
    float e = kit__inst_hit(t, 0.002, decay);
    float amp = e * e;
    index = clamp(index, 0.0, 10.0);
    float fm = index * amp * kit__inst_band(hz * (1.0 + 1.4 * index)) * kit__inst_sin(1.4 * hz * t);
    return amp * sin(6.28318531 * fract(hz * t) + fm);
}
