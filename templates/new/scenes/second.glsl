void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec3 col = 0.5 + 0.5 * cos(iTime + uv.xyx * 3.0 + vec3(0.0, 2.0, 4.0));
    // iShotProgress runs 0..1 over the shot: fade to black at the very end.
    col *= smoothstep(1.0, 0.85, iShotProgress);
    fragColor = vec4(col, 1.0);
}
