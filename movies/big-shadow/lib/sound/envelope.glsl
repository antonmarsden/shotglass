// Big Shadow, lib/sound: envelope.glsl
float bs_env_fall(float x) {
    x = 1.0 - clamp(x, 0.0, 1.0);
    return x * x;
}

float bs_env_ar(float t, float attack, float release) {
    if (t < 0.0) return 0.0;
    if (t < attack) return t / attack;
    return bs_env_fall((t - attack) / max(release, 1e-6));
}
