// Shot 2: kit fbm clouds, framed by a growing ring from the films' lib/.
#include <kit/noise.glsl>
#include <kit/color.glsl>
#include <films/shapes.glsl>

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 p = (2.0 * fragCoord - iResolution.xy) / iResolution.y;
    float n = kit_fbm(vec3(1.5 * p, 0.2 * iTime), 5);
    vec3 col = mix(vec3(0.10, 0.12, 0.25), vec3(0.85, 0.80, 0.95), n);
    col = mix(col, vec3(1.0, 0.85, 0.4), films_ring(p, uRadius, 0.03, 2.0 / iResolution.y));
    fragColor = vec4(kit_srgb_encode(kit_aces(col)), 1.0);
}
