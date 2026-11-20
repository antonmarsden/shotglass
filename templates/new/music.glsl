// Music: stereo sample at `time` seconds of the movie (it runs across the cuts). CUE_<SHOT>_START/_SETTLED/_END and
// MOVIE_DURATION are defined for you, so the music can follow the edit.
#define TAU 6.2831853

vec2 mainSound(int samp, float time) {
    // a soft drone that changes key when the second shot arrives
    float k = smoothstep(CUE_SECOND_START, CUE_SECOND_SETTLED, time);
    float f = mix(110.0, 146.83, k);
    float s = 0.25 * sin(TAU * f * time) + 0.12 * sin(TAU * f * 1.5 * time + sin(time));
    // fade in and out
    s *= smoothstep(0.0, 1.0, time) * smoothstep(MOVIE_DURATION, MOVIE_DURATION - 1.5, time);
    return vec2(s * 0.9, s);
}
