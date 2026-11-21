// Music: a plucked note every half second, stepping up a pentatonic scale, from the kit's
// sound library. CUE_CLOUDS_START comes from the edit (the cut to the second shot).
#include <sound/note.glsl>
#include <sound/envelope.glsl>
#define TAU 6.2831853

vec2 mainSound(int samp, float time) {
    float beat = 0.5;
    float n = floor(time / beat);
    float t = time - n * beat;
    int root = time < CUE_CLOUDS_START ? 57 : 62;   // A3, then D4 after the cut
    float hz = kit_note_hz(kit_scale_pentatonic(int(mod(n, 8.0)), root));
    float s = 0.2 * sin(TAU * hz * t) * kit_env_ar(t, 0.005, 0.4);
    s *= smoothstep(MOVIE_DURATION, MOVIE_DURATION - 1.0, time);
    return vec2(s * 0.8, s);
}
