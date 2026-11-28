// Two insets over the picture: the layer's footage (iChannel0) top left and the
// buffer (iChannel1, black until it starts at 1.5 s) top right; clear elsewhere.
void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    fragColor = vec4(0.0);
    if (uv.y > 0.6) {
        if (uv.x < 0.4) {
            fragColor = vec4(texture(iChannel0, vec2(uv.x / 0.4, (uv.y - 0.6) / 0.4)).rgb, 1.0);
        } else if (uv.x > 0.6) {
            fragColor = vec4(texture(iChannel1, vec2((uv.x - 0.6) / 0.4, (uv.y - 0.6) / 0.4)).rgb, 1.0);
        }
    }
}
