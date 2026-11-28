// The plate with the matte over it (straight alpha), iChannelTime[0] as a bar along
// the bottom (1 s = the full width), and a corner that is red while
// iChannelResolution[0] is the plate's 64×36 (it stays so while the plate reads black).
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec3 col = texture(iChannel0, uv).rgb;
    vec4 m = texture(iChannel1, uv);
    col = mix(col, m.rgb, m.a);
    if (uv.y < 0.04) {
        col = uv.x < iChannelTime[0] ? vec3(0.2, 1.0, 0.3) : vec3(0.1);
    }
    if (uv.y > 0.92 && uv.x > 0.94) {
        col = iChannelResolution[0].x == 64.0 ? vec3(1.0, 0.2, 0.2) : vec3(0.0, 0.0, 1.0);
    }
    fragColor = vec4(col, 1.0);
}
