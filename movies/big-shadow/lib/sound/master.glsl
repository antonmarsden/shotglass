// Big Shadow, lib/sound: master.glsl
float bs_duck(float level, float lo, float hi, float depth) {
    return 1.0 - clamp(depth, 0.0, 1.0) * smoothstep(lo, max(hi, lo + 1e-6), level);
}

float bs_soft_clip(float x, float ceiling) {
    float c = max(ceiling, 1e-6);
    float k = 0.5 * c;   // the knee
    float a = abs(x);
    if (a <= k) return x;
    float y = k + (c - k) * tanh(min((a - k) / (c - k), 10.0));
    return x < 0.0 ? -y : y;
}
vec2 bs_soft_clip(vec2 s, float ceiling) {
    return vec2(bs_soft_clip(s.x, ceiling), bs_soft_clip(s.y, ceiling));
}
