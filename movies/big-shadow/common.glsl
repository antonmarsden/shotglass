// Shared helpers for the Big Shadow scenes. Include with:  #include "../common.glsl"
// Noise, shapes and tonemapping come from shotglass-kit (see [libraries] in movie.toml).

#include <kit/noise.glsl>
#include <kit/sdf.glsl>
#include <kit/color.glsl>

// 1D smooth noise in 0..1 (flame wobble and flicker).
float noise1(float x){ return kit_value_noise(vec2(x, 0.0)); }
mat2 rot(float a){ float c=cos(a), s=sin(a); return mat2(c,s,-s,c); }   // counter-clockwise
