// ============================================================
//  MOON — a 2D night skyline. Cheap to render (no raymarching).
//  Uniforms:
//    uMoon   moon centre in screen uv (0..1); the iris into the next shot opens here
//    uWind   cloud drift speed
// ============================================================
#include "../common.glsl"

// Roof line of the skyline at horizontal position x (aspect units). Returns height.
float roofHeight(float x, out float id){
  const float W = 0.23;
  float i = floor(x / W);
  float f = fract(x / W);
  id = i;
  float base = 0.11 + 0.14 * hash11(i * 3.1 + 1.0);
  float h = base;
  // gable roofs on some houses
  if (hash11(i * 7.7) > 0.35) h += 0.07 * (1.0 - abs(f * 2.0 - 1.0));
  // chimneys
  float cx = 0.2 + 0.5 * hash11(i * 5.3);
  if (hash11(i * 2.9 + 4.0) > 0.4 && abs(f - cx) < 0.045) h = max(h, base + 0.055 + 0.03 * hash11(i));
  // small gaps between houses
  if (f < 0.02 || f > 0.98) h = 0.06;
  return h;
}

// 2D mouse silhouette sitting on a roof, facing +x. Returns signed distance.
float sdMouse2(vec2 p, float t){
  float d = length((p - vec2(0.0, 0.020)) / vec2(0.024, 0.021)) - 1.0;           // body
  d *= 0.02;
  d = min(d, length(p - vec2(0.020, 0.040)) - 0.013);                              // head
  d = min(d, length((p - vec2(0.034, 0.036)) / vec2(1.6, 1.0)) - 0.0065);           // snout
  d = min(d, length(p - vec2(0.012, 0.055)) - 0.0075);                             // ear
  d = min(d, length(p - vec2(0.022, 0.057)) - 0.0065);                             // ear
  // tail: hangs over the roof edge and swishes
  float tail = 1e9;
  vec2 prev = vec2(-0.020, 0.008);
  for (int i = 1; i <= 8; i++){
    float s = float(i) / 8.0;
    vec2 cur = vec2(-0.020 - 0.045 * s, 0.008 - 0.05 * s * s + 0.012 * s * sin(t * 2.2 - s * 3.0));
    vec2 pa = p - prev, ba = cur - prev;
    float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    tail = min(tail, length(pa - ba * h) - mix(0.0028, 0.0012, s));
    prev = cur;
  }
  return min(d, tail);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord){
  float aspect = iResolution.x / iResolution.y;
  vec2 p = fragCoord / iResolution.y;            // x in [0, aspect], y in [0, 1]
  float px = 1.0 / iResolution.y;
  vec2 mc = uMoon * vec2(aspect, 1.0);
  float dm = length(p - mc);

  // sky
  vec3 col = mix(vec3(0.10, 0.08, 0.20), vec3(0.015, 0.025, 0.07), smoothstep(0.0, 0.9, p.y));
  col += vec3(0.25, 0.28, 0.40) * exp(-dm * 5.0) * 0.5;   // moon glow

  // stars
  vec2 sg = p * 70.0;
  vec2 si = floor(sg);
  float sh = hash21(si);
  if (sh > 0.82){
    vec2 sp = si + 0.2 + 0.6 * vec2(hash21(si + 3.1), hash21(si + 7.7));
    float tw = 0.6 + 0.4 * sin(iTime * (1.5 + 3.0 * sh) + sh * 40.0);
    float star = smoothstep(1.6, 0.0, length(sg - sp) * 14.0 * (1.0 - sh + 0.25));
    col += vec3(0.9, 0.92, 1.0) * star * tw * smoothstep(0.25, 0.5, p.y) * smoothstep(0.08, 0.2, dm);
  }

  // moon with craters
  float R = 0.085;
  float disc = smoothstep(R + px, R - px, dm);
  if (disc > 0.0){
    vec2 q = (p - mc) / R;
    float cr = fbm(q * 3.0 + 4.0);
    float limb = sqrt(max(0.0, 1.0 - dot(q, q)));
    vec3 mcol = vec3(0.98, 0.94, 0.82) * (0.78 + 0.22 * limb) * (0.82 + 0.3 * cr);
    mcol *= 1.0 - 0.18 * smoothstep(0.55, 0.62, vnoise(q * 2.2 + 1.3));   // maria
    col = mix(col, mcol, disc);
  }

  // clouds drifting across the moon
  vec2 cp = vec2(p.x * 1.4 + iTime * uWind, p.y * 4.0);
  float cl = fbm(cp + vec2(0.0, fbm(cp * 0.7 + iTime * 0.02)));
  float band = smoothstep(0.45, 0.75, p.y) * smoothstep(1.0, 0.8, p.y);
  float cloud = smoothstep(0.52, 0.8, cl) * band * 0.8;
  vec3 ccol = mix(vec3(0.12, 0.12, 0.22), vec3(0.75, 0.75, 0.85), exp(-dm * 4.0));
  col = mix(col, ccol, cloud);

  // skyline
  float id;
  float h = roofHeight(p.x, id);
  float build = smoothstep(h + px, h - px, p.y);
  if (build > 0.0){
    vec3 bcol = mix(vec3(0.02, 0.02, 0.05), vec3(0.05, 0.05, 0.10), p.y / max(h, 1e-3));
    // lit windows
    vec2 wg = vec2(p.x * 26.0, p.y * 34.0);
    vec2 wi = floor(wg), wf = fract(wg);
    float lit = step(0.78, hash21(wi + id * 13.0));
    float win = step(0.3, wf.x) * step(wf.x, 0.75) * step(0.25, wf.y) * step(wf.y, 0.75);
    win *= step(p.y, h - 0.03) * step(0.03, p.y);
    float flick = 0.85 + 0.15 * sin(iTime * 0.7 + hash21(wi) * 30.0);
    bcol += vec3(1.0, 0.72, 0.35) * win * lit * 0.55 * flick;
    col = mix(col, bcol, build);
  }

  // our mouse on a rooftop, gazing at the moon
  float mx = 0.34 * aspect;
  float hid;
  float mh = roofHeight(mx, hid);
  float dmouse = sdMouse2(p - vec2(mx, mh - 0.002), iTime);
  col = mix(col, vec3(0.005, 0.005, 0.015), smoothstep(px, -px, dmouse));

  // vignette & grain
  vec2 v = fragCoord / iResolution.xy;
  col *= 0.6 + 0.4 * pow(16.0 * v.x * v.y * (1.0 - v.x) * (1.0 - v.y), 0.2);
  col += (hash21(fragCoord + fract(iTime) * 100.0) - 0.5) * 0.012;
  fragColor = vec4(pow(max(col, 0.0), vec3(0.92)), 1.0);
}
