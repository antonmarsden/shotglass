// Clicks: a sample whose second difference (the jump in slope) is far above the
// local level of the second difference around it, ±WIN samples. A discontinuity
// (a cut without a fade, a wrapped phase, a hard gate) scores high; bright but
// smooth sound doesn't, as its neighbours are as rough. FLOOR keeps near-silence
// from scoring on rounding noise.
#ifndef WIN
#define WIN 240
#endif
#ifndef STEP
#define STEP 8
#endif
#ifndef FLOOR
#define FLOOR 0.001
#endif

float roughness(int s) {
    vec2 d = getSound(s) - 2.0 * getSound(s - 1) + getSound(s - 2);
    return max(abs(d.x), abs(d.y));
}

void mainSoundMetric(out float value, in int samp) {
    float e = 0.0;
    float n = 0.0;
    for (int k = -WIN; k <= WIN; k += STEP) {
        if (abs(k) < 4) continue;
        float r = roughness(samp + k);
        e += r * r;
        n += 1.0;
    }
    value = roughness(samp) / (sqrt(e / n) + FLOOR);
}
