// A sound that belongs to one shot: `time` is shot-local (like iTime), and the
// shot's uniforms are available. It fades with the shot's transitions.
#define TAU 6.2831853

vec2 mainSound(int samp, float time) {
    // a chime every second, decaying
    float dt = mod(max(time, 0.0), 1.0);
    float f = 880.0;
    float s = sin(TAU * f * dt) * exp(-dt * 6.0) * 0.15;
    return vec2(s * 0.7, s);
}
