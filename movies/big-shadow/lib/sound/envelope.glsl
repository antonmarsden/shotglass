// shotglass-kit/sound: envelope.glsl
// (a copy from shotglass-kit 48313cf, cut down to what Big Shadow uses)
//
// Envelopes: how loud a note is at a time since it started. Each takes `t`, seconds
// since the note began (negative: not started yet, gives 0), and returns a level in
// 0..1 to multiply a voice by. They hold no state, so any sample can be computed on
// its own. Attacks rise in a straight line; decays and releases fall along
// (1 - x)^2, which sounds close to an exponential fade but reaches its target
// exactly, so a note ends in true silence at a known time.
// sound: yes

float kit__env_fall(float x) {
    x = 1.0 - clamp(x, 0.0, 1.0);
    return x * x;
}

// kit_env_ar(t, attack, release): rise, then fall straight away. Plucks, hits, blips.
//   t        seconds since the note began
//   attack   seconds to reach 1 (0 for an instant start)
//   release  seconds to fall from 1 to 0 after the attack (0 cuts off)
//   ->  0..1; 0 before t = 0 and from t = attack + release on
//   e.g. float v = kit_env_ar(since, 0.005, 0.3) * sin(6.28318531 * 440.0 * since);
float kit_env_ar(float t, float attack, float release) {
    if (t < 0.0) return 0.0;
    if (t < attack) return t / attack;
    return kit__env_fall((t - attack) / max(release, 1e-6));
}
