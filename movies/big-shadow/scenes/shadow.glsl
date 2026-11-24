// ============================================================
//  BIG SHADOW — raymarched SDF scene with soft point-light shadows.
//  Uniforms (set per shot in movie.toml):
//    uDist    distance from the flame to the mouse (along z)
//    uRadius  flame radius -> penumbra width
//    uFlicker 0..1 flame flicker amount
//    uLight   0..1 candle brightness (0 = blown out)
//    uOrbit   camera orbit (yaw, pitch) in radians
//    uZoom    lens zoom (zooming in swings the aim onto the mouse)
// ============================================================
#include "../common.glsl"

const float MOUSE_Z = 2.6;          // mouse stands 2.6 units in front of the wall (z = 0)
const float FLAME_Y = 0.038;        // flame height: below the mouse's middle, so the shadow climbs

// ---------- the light ----------
vec3 lightPos(){
  // the flame wobbles a few millimetres; the shadow amplifies the wobble
  float wob = uFlicker*(noise1(iTime*6.0)-0.5)*0.010;
  float lift = uFlicker*(noise1(iTime*9.0+7.0)-0.5)*0.004;
  return vec3(wob, FLAME_Y+lift, MOUSE_Z+uDist);
}
float lightPower(){ return uLight * (1.0 - uFlicker*0.22*noise1(iTime*11.0+3.0)); }

// ---------- the mouse (local space: feet at origin, facing +x) ----------
// returns (distance, material): 3 fur, 4 pink skin, 6 eye
vec2 mouseSDF(vec3 q){
  float t = iTime;
  // cheap bounding sphere: skip the detail when we're far away
  float bound = length(q-vec3(-0.04,0.09,0.0)) - 0.22;
  if(bound > 0.03) return vec2(bound, 3.0);

  float breathe = 1.0 + 0.025*sin(t*3.1);
  // body, rearing up and leaning forward
  vec3 b = q-vec3(0.004,0.075,0.0); b.xy = rot(-0.32)*b.xy;
  float d = bs_sd_ellipsoid(b, vec3(0.033*breathe,0.056,0.030*breathe));
  // haunches
  d = bs_op_smin(d, bs_sd_ellipsoid(q-vec3(-0.010,0.030,0.0), vec3(0.032,0.028,0.034)), 0.016);
  // head with a pointy snout, tilted down a touch
  float bob = 0.004*sin(t*1.7);
  vec3 h = q-vec3(0.034,0.138+bob,0.0); h.xy = rot(-0.22)*h.xy;
  float dh = bs_sd_ellipsoid(h, vec3(0.036,0.024,0.023));
  dh = bs_op_smin(dh, bs_sd_ellipsoid(h-vec3(0.022,-0.004,0.0), vec3(0.022,0.012,0.012)), 0.01);
  d = bs_op_smin(d, dh, 0.014);

  // arms raised in a "fear me" pose
  float claw = 0.007*sin(t*2.3);
  for(int s=-1; s<=1; s+=2){
    float z = float(s)*0.019;
    d = bs_op_smin(d, bs_sd_capsule(q, vec3(0.022,0.098,z), vec3(0.056,0.112+claw,z*1.1), 0.0065), 0.008);
  }
  vec2 res = vec2(d, 3.0);

  // ---- pink parts ----
  float pk = 1e9;
  for(int s=-1; s<=1; s+=2){
    float z = float(s)*0.019;
    // ears: thin discs facing the light, offset in x so both show in profile
    vec3 e = q-vec3(0.012+0.010*float(s), 0.170+bob, z*0.85);
    pk = min(pk, bs_sd_ellipsoid(e, vec3(0.018,0.020,0.0035)));
    // paws with little fingers
    vec3 pw = vec3(0.060,0.115+claw,z*1.1);
    pk = min(pk, bs_sd_sphere(q-pw, 0.0068));
    for(int f=0; f<3; f++){
      float a = 0.6 - float(f)*0.55;
      pk = min(pk, bs_sd_capsule(q, pw, pw+vec3(cos(a),sin(a),0.0)*0.012, 0.0024));
    }
    // hind feet
    pk = min(pk, bs_sd_capsule(q, vec3(-0.002,0.004,z*1.1), vec3(0.036,0.004,z*1.2), 0.0045));
  }
  // nose
  vec3 nosePos = vec3(0.072,0.130+bob,0.0);
  pk = min(pk, bs_sd_sphere(q-nosePos, 0.0055));
  // tail: a chain of tapering capsules, swishing in an S-curve
  vec3 prev = vec3(-0.036,0.022,0.0);
  for(int i=1; i<=10; i++){
    float s = float(i)/10.0;
    vec3 cur = vec3(-0.036-0.14*s,
                    0.014 + s*s*(0.05+0.03*sin(t*2.0-s*4.0)),
                    0.035*s*sin(t*1.3+s*3.0));
    pk = min(pk, bs_sd_capsule(q, prev, cur, mix(0.0058,0.0016,s)));
    prev = cur;
  }
  if(pk < res.x) res = vec2(pk, 4.0);

  // whiskers (very thin — they become fuzzy smudges in a soft shadow)
  float wk = 1e9;
  for(int s=-1; s<=1; s+=2)
  for(int k=-1; k<=1; k++){
    vec3 a = nosePos+vec3(-0.004,-0.002,float(s)*0.006);
    vec3 b2 = a+vec3(0.030,0.010*float(k)+0.002*sin(t*5.0+float(k)),float(s)*0.020);
    wk = min(wk, bs_sd_capsule(q, a, b2, 0.0007));
  }
  if(wk < res.x) res = vec2(wk, 3.5);

  // eyes
  for(int s=-1; s<=1; s+=2){
    float de = bs_sd_sphere(q-vec3(0.052,0.146+bob,float(s)*0.0165), 0.0048);
    if(de < res.x) res = vec2(de, 6.0);
  }
  return res;
}

// tealight sitting under the flame
float tealightSDF(vec3 p){
  vec3 c = p-vec3(0.0,0.0105,MOUSE_Z+uDist);
  float cup = bs_sd_cylinder(c, 0.028, 0.0105)-0.0008;
  float wick = bs_sd_capsule(p, vec3(0.0,0.02,MOUSE_Z+uDist), vec3(0.0,0.029,MOUSE_Z+uDist), 0.0011);
  return min(cup, wick);
}

// things that can block the light (floor & wall can't, so they're left out)
float mapShadow(vec3 p){
  return min(mouseSDF(p-vec3(0.0,0.0,MOUSE_Z)).x, tealightSDF(p));
}

// the whole scene: (distance, material) — 1 floor, 2 wall, 7 tealight
vec2 map(vec3 p){
  vec2 res = vec2(p.y, 1.0);
  if(p.z < res.x) res = vec2(p.z, 2.0);
  float tl = tealightSDF(p);
  if(tl < res.x) res = vec2(tl, 7.0);
  vec2 m = mouseSDF(p-vec3(0.0,0.0,MOUSE_Z));
  if(m.x < res.x) res = m;
  return res;
}

vec3 calcNormal(vec3 p){
  const vec2 e = vec2(1.0,-1.0)*0.00025;
  return normalize(e.xyy*map(p+e.xyy).x + e.yyx*map(p+e.yyx).x +
                   e.yxy*map(p+e.yxy).x + e.xxx*map(p+e.xxx).x);
}

// ---------- soft shadow toward a spherical light ----------
// At distance t along the ray the cone of rays reaching a light of radius R
// (distance L away) has radius R*t/L; h/(w*t), w = R/L, is how much of it stays clear.
float softShadow(vec3 ro, vec3 rd, float tmax, float w){
  float res = 1.0;
  float t = 0.002;
  for(int i=0; i<160; i++){
    float h = mapShadow(ro+rd*t);
    res = min(res, h/(w*t));
    t += clamp(h, 0.0012, 0.08);
    if(res < -1.0 || t > tmax) break;
  }
  res = max(res, -1.0);
  return 0.25*(1.0+res)*(1.0+res)*(2.0-res);   // smooth remap of [-1,1] -> [0,1]
}

float calcAO(vec3 p, vec3 n){
  float occ = 0.0, sca = 1.0;
  for(int i=0; i<5; i++){
    float h = 0.004 + 0.03*float(i)/4.0;
    occ += (h - map(p+n*h).x)*sca;
    sca *= 0.8;
  }
  return clamp(1.0-3.0*occ, 0.0, 1.0);
}

// A dark line of half-width a at distance d, seen through a pixel footprint w:
// widened by the footprint and faded so its total darkness stays the same. Lines
// thinner than a pixel would otherwise flicker on and off with the tiniest change in
// the hit point (and land on different pixels on different drivers).
float seamLine(float d, float a, float w){
  return (1.0-smoothstep(0.0, a+w, d))*a/(a+w);
}

// fp: the size of a pixel at p (world units, across the ray); rd: the view ray.
vec3 albedo(vec3 p, float m, vec3 n, float fp, vec3 rd){
  if(m < 1.5){                                     // wooden floor boards running to the wall
    float bw = 0.16, bl = 1.4;                     // board width, board length
    float wx = fp;                                 // footprint across the boards
    float wz = fp/max(abs(rd.y), 0.05);            // and along them (grazing: stretched)
    float id = floor(p.x/bw);
    float gx = fract(p.x/bw);
    float zoff = bs_hash(ivec2(int(id), 6))*3.0;
    float u = (p.z+zoff)/bl;
    float k = floor(u+0.5);                        // the nearest board end
    float sz = (u-k)*bl;                           // signed distance to it
    float grain = bs_fbm(vec2(p.x*30.0, p.z*2.0+id*7.0), 4);
    vec3 c = mix(vec3(0.28,0.16,0.08), vec3(0.42,0.26,0.13), grain);
    // each board's shade, blended across its end over the footprint
    float shade = mix(bs_hash(ivec2(int(id), int(k)-1)), bs_hash(ivec2(int(id), int(k))),
                      smoothstep(-wz, wz, sz));
    c *= 0.8+0.35*shade;
    float dark = max(seamLine(min(gx, 1.0-gx)*bw, 0.0032, wx),
                     seamLine(abs(sz), 0.003, wz));
    return c*(1.0-0.75*dark);
  }
  if(m < 2.5){                                     // plaster wall
    float n1 = bs_fbm(p.xy*6.0, 4);
    vec3 c = vec3(0.60,0.55,0.48)*(0.88+0.2*n1);
    float sk = step(p.y,0.09);                     // skirting board
    return mix(c, vec3(0.30,0.24,0.18), sk);
  }
  if(m < 3.2) return vec3(0.31,0.25,0.20)*(0.85+0.3*bs_value_noise(p.xy*400.0));   // fur
  if(m < 3.7) return vec3(0.75);                                           // whiskers
  if(m < 4.5) return vec3(0.78,0.47,0.46);                                 // pink skin
  if(m < 6.5) return vec3(0.01);                                           // eye
  return vec3(0.75,0.74,0.72);                                             // tealight
}

void mainImage(out vec4 fragColor, in vec2 fragCoord){
  vec2 uv = (2.0*fragCoord - iResolution.xy)/iResolution.y;

  // camera: behind and to the right of the candle, looking at the wall
  vec3 ta = vec3(0.0, 0.62, 1.2);
  vec3 taMouse = vec3(0.0, 0.08, MOUSE_Z);
  float zk = smoothstep(1.3, 4.0, uZoom);
  vec3 off = vec3(0.95, -0.15, 3.55);
  off.xz = rot(uOrbit.x)*off.xz;
  off.yz = rot(uOrbit.y)*off.yz;
  vec3 ro = ta + off;
  ro.y = max(ro.y, 0.05);
  ta = mix(ta, taMouse, zk);
  vec3 ww = normalize(ta-ro);
  vec3 uu = normalize(cross(ww, vec3(0,1,0)));
  vec3 vv = cross(uu, ww);
  vec3 rd = normalize(uv.x*uu + uv.y*vv + 1.45*uZoom*ww);

  // ---- raymarch ----
  float t = 0.0; float m = -1.0;
  for(int i=0; i<220; i++){
    vec2 h = map(ro+rd*t);
    if(abs(h.x) < 0.00015*t + 0.00002){ m = h.y; break; }
    t += h.x*0.9;
    if(t > 30.0) break;
  }
  // The floor and the wall are planes: put the hit exactly on them. Where the march
  // stops within its tolerance depends on the driver's rounding, and where the view
  // grazes a plane that slides the hit point a long way (different grain, different
  // shadow).
  if(m > 0.0 && m < 1.5 && rd.y < -1e-4) t = -ro.y/rd.y;
  else if(m > 1.5 && m < 2.5 && rd.z < -1e-4) t = -ro.z/rd.z;

  vec3 L = lightPos();
  vec3 flameCol = vec3(1.0,0.60,0.26);
  float power = lightPower();
  vec3 col = vec3(0.0);

  if(m > 0.0){
    vec3 p = ro+rd*t;
    vec3 n = calcNormal(p);
    float fp = t*2.0/(iResolution.y*1.45*uZoom);   // pixel size at the hit
    vec3 alb = albedo(p, m, n, fp, rd);
    vec3 ld = L-p; float dl = length(ld); ld /= dl;

    float dif = max(dot(n, ld), 0.0);
    if(m > 2.9 && m < 4.6) dif = max(0.85*dot(n,ld)+0.15, 0.0);   // a little fur wrap
    float sha = (dif > 0.0 && power > 0.001) ? softShadow(p+n*0.0015, ld, dl-0.006, uRadius/dl) : 0.0;
    float att = 1.0/(0.8 + dl*dl);        // softened inverse-square so the mouse isn't blown out
    float ao  = calcAO(p, n);

    vec3 lin = flameCol*power*6.5*att*dif*sha;
    lin += vec3(0.020,0.024,0.034)*ao;                 // faint cool room light
    float bounce = clamp(0.5-0.5*n.y,0.0,1.0)*0.8/(0.1+dl*dl);
    lin += flameCol*0.08*bounce*ao*power;              // warm bounce from the lit floor
    col = alb*lin;

    if(m > 5.5){                                       // specular on eyes & tealight
      vec3 hv = normalize(ld-rd);
      float sp = pow(max(dot(n,hv),0.0), m > 6.5 ? 40.0 : 120.0);
      col += flameCol*sp*sha*att*power*(m > 6.5 ? 0.6 : 4.0);
    }
    col = mix(col, vec3(0.0), 1.0-exp(-0.004*t*t));
  }

  // ---- the flame itself: an additive glowing teardrop + halo, shrinking as uLight drops ----
  float tc = dot(L-ro, rd);
  if(tc > 0.0 && (m < 0.0 || tc < t) && uLight > 0.001){
    vec3 q = ro+rd*tc - L;
    float flick = 1.0+0.15*uFlicker*(noise1(iTime*13.0)-0.5);
    float sz = mix(0.35, 1.0, sqrt(uLight));
    vec3 qs = q/(vec3(0.0042, 0.0105*flick, 0.0042)*sz);
    qs.y -= 0.35;
    float r = length(qs);
    float core = exp(-r*r*1.6);
    vec3 flame = mix(vec3(1.0,0.45,0.1), vec3(1.0,0.95,0.8), smoothstep(0.2,0.9,core))*core*6.0*uLight;
    float d2 = dot(q,q);
    vec3 halo = flameCol*0.0009/(d2+0.0003)*power;
    col += flame + halo;
  }

  col = bs_aces(col*1.15);
  col = pow(col, vec3(1.0/2.2));
  vec2 v = fragCoord/iResolution.xy;
  col *= 0.55+0.45*pow(16.0*v.x*v.y*(1.0-v.x)*(1.0-v.y), 0.18);
  fragColor = vec4(col, 1.0);
}
