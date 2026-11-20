// Shared helpers for the Big Shadow scenes. Include with:  #include "../common.glsl"

float hash11(float n){ return fract(sin(n)*43758.5453123); }
float hash21(vec2 p){ return fract(sin(dot(p, vec2(127.1,311.7)))*43758.5453123); }
float noise1(float x){ float i=floor(x), f=fract(x); f=f*f*(3.0-2.0*f); return mix(hash11(i), hash11(i+1.0), f); }
float vnoise(vec2 p){
  vec2 i=floor(p), f=fract(p); f=f*f*(3.0-2.0*f);
  return mix(mix(hash21(i), hash21(i+vec2(1,0)), f.x), mix(hash21(i+vec2(0,1)), hash21(i+vec2(1,1)), f.x), f.y);
}
float fbm(vec2 p){ float a=0.5, s=0.0; for(int i=0;i<4;i++){ s+=a*vnoise(p); p*=2.03; a*=0.5; } return s; }
mat2 rot(float a){ float c=cos(a), s=sin(a); return mat2(c,s,-s,c); }   // counter-clockwise

// SDF primitives (Inigo Quilez)
float smin(float a, float b, float k){ float h=clamp(0.5+0.5*(b-a)/k,0.0,1.0); return mix(b,a,h)-k*h*(1.0-h); }
float sdEllipsoid(vec3 p, vec3 r){ float k0=length(p/r), k1=length(p/(r*r)); return k0*(k0-1.0)/k1; }
float sdSphere(vec3 p, float r){ return length(p)-r; }
float sdCapsule(vec3 p, vec3 a, vec3 b, float r){
  vec3 pa=p-a, ba=b-a; float h=clamp(dot(pa,ba)/dot(ba,ba),0.0,1.0); return length(pa-ba*h)-r;
}
float sdCylY(vec3 p, float r, float h){
  vec2 d=abs(vec2(length(p.xz), p.y))-vec2(r,h); return min(max(d.x,d.y),0.0)+length(max(d,0.0));
}

vec3 aces(vec3 x){ return clamp((x*(2.51*x+0.03))/(x*(2.43*x+0.59)+0.14), 0.0, 1.0); }
