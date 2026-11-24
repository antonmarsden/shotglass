// shotglass-kit/core: sdf.glsl
// (a copy from shotglass-kit 48313cf, cut down to what Big Shadow uses)
//
// Signed distance functions: the distance from p to a shape's surface, negative
// inside. Build a scene by combining them with the kit_op_ functions, then draw it
// in 2D (smoothstep on the distance) or raymarch it in 3D.
// Shapes are centred on the origin: move them by passing p - position.
// Lengths (radii, half-sizes, fillets) are in the units of p.
// sound: yes

// ---- 3D -------------------------------------------------------------------------

// kit_sd_sphere(p, r): sphere of radius r.
float kit_sd_sphere(vec3 p, float r) { return length(p) - r; }

// kit_sd_capsule(p, a, b, r): a sausage from a to b with radius r.
float kit_sd_capsule(vec3 p, vec3 a, vec3 b, float r) {
    vec3 pa = p - a, ba = b - a;
    float h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-12), 0.0, 1.0);
    return length(pa - ba * h) - r;
}

// kit_sd_cylinder(p, r, h): upright (y axis) cylinder with radius r, height 2h.
float kit_sd_cylinder(vec3 p, float r, float h) {
    vec2 d = abs(vec2(length(p.xz), p.y)) - vec2(r, h);
    return min(max(d.x, d.y), 0.0) + length(max(d, 0.0));
}

// kit_sd_ellipsoid(p, r): ellipsoid with radii r. A bound, not exact: fine for
// raymarching and shading, but the distance is only exact on the surface.
float kit_sd_ellipsoid(vec3 p, vec3 r) {
    float k0 = length(p / r);
    float k1 = length(p / (r * r));
    return k0 * (k0 - 1.0) / max(k1, 1e-12);
}

// ---- combining (work in 2D and 3D) ------------------------------------------------

// kit_op_smin(a, b, k): union with a smooth fillet of size k (k = 0: plain min).
//   e.g. float blob = kit_op_smin(kit_sd_sphere(p, 1.0), kit_sd_sphere(p - off, 0.7), 0.3);
float kit_op_smin(float a, float b, float k) {
    if (k <= 0.0) return min(a, b);
    float h = max(k - abs(a - b), 0.0) / k;
    return min(a, b) - h * h * k * 0.25;
}
