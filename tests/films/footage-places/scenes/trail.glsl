// The brighter of its footage (iChannel1) and its decayed previous state.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    fragColor = vec4(max(texture(iChannel1, uv).rgb, texture(iChannel0, uv).rgb * 0.85), 1.0);
}
