// Shadertoy-style: write mainImage. iTime is the shot's own clock.
// uHue comes from movie.toml; no need to declare it.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = (2.0 * fragCoord - iResolution.xy) / iResolution.y;
    float r = length(uv);
    float rings = 0.5 + 0.5 * cos(12.0 * r - 3.0 * iTime);
    vec3 col = 0.5 + 0.5 * cos(6.2831 * (uHue + vec3(0.0, 0.33, 0.67)) + r * 2.0);
    fragColor = vec4(col * rings * smoothstep(1.4, 0.2, r), 1.0);
}
