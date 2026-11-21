// films/lib: shapes.glsl
//
// 2D shapes the films are trying out. Written to shotglass-kit's CONVENTIONS.md from the
// start (prefix, header contract, units), so graduating is a move and a rename.
// sound: yes

// films_ring(p, r, width, softness): coverage of a ring (an annulus) centred on 0.
//   p         position, in frame heights (a circle stays round at any aspect ratio)
//   r         radius of the ring's centre line, in the units of p
//   width     full thickness of the ring, in the units of p (>= 0)
//   softness  edge blur, in the units of p (> 0 gives an anti-aliased edge)
//   ->        0..1: 1 on the ring, 0 away from it
//   cost: a length and a smoothstep
//   e.g. col = mix(col, vec3(1.0), films_ring(uv, 0.4, 0.02, 2.0 / iResolution.y));
float films_ring(vec2 p, float r, float width, float softness) {
    float d = abs(length(p) - r) - 0.5 * max(width, 0.0);
    return 1.0 - smoothstep(-0.5 * softness, 0.5 * softness, d);
}
