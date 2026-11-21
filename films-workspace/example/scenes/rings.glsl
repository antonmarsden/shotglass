// Shot 1: a ring from the films' lib/ over a kit palette, with the kit's vignette.
#include <kit/color.glsl>
#include <films/shapes.glsl>

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 p = (2.0 * fragCoord - iResolution.xy) / iResolution.y;
    vec2 uv = fragCoord / iResolution.xy;
    vec3 bg = kit_palette(uHue + 0.15 * p.y, vec3(0.5), vec3(0.4), vec3(1.0),
                          vec3(0.0, 0.33, 0.67));
    float r = 0.3 + 0.08 * sin(1.5 * iTime);
    float ring = films_ring(p, r, 0.05, 2.0 / iResolution.y);
    vec3 col = mix(bg * 0.6, vec3(1.0), ring);
    fragColor = vec4(col * kit_vignette(uv, 0.5, 0.6), 1.0);
}
