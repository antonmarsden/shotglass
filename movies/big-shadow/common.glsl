// Shared helpers for the Big Shadow scenes. Include with:  #include "../common.glsl"
// Noise, shapes and tonemapping come from shotglass-kit (see [libraries] in movie.toml).

#include <kit/noise.glsl>
#include <kit/sdf.glsl>
#include <kit/color.glsl>

// 1D smooth noise in 0..1 (flame wobble and flicker).
float noise1(float x){ return kit_value_noise(vec2(x, 0.0)); }
// sin and cos to float precision (Taylor series, |x| <= pi after range reduction). The
// built-ins can be much rougher: the Apple Software Renderer's cos is ~5e-4 off, which
// swings the zoomed-in close-up camera by a third of a pixel and moves every point
// where the view grazes the floor or the wall.
vec2 sincosPrecise(float x){
  x -= 6.28318531*floor(x/6.28318531 + 0.5);
  float x2 = x*x;
  float s = x*(1.0 + x2*(-1.0/6.0 + x2*(1.0/120.0 + x2*(-1.0/5040.0 + x2*(1.0/362880.0
          + x2*(-1.0/39916800.0 + x2*(1.0/6227020800.0 + x2*(-1.0/1307674368000.0
          + x2*(1.0/355687428096000.0)))))))));
  float c = 1.0 + x2*(-0.5 + x2*(1.0/24.0 + x2*(-1.0/720.0 + x2*(1.0/40320.0
          + x2*(-1.0/3628800.0 + x2*(1.0/479001600.0 + x2*(-1.0/87178291200.0
          + x2*(1.0/20922789888000.0))))))));
  return vec2(s, c);
}
mat2 rot(float a){ vec2 sc = sincosPrecise(a); return mat2(sc.y,sc.x,-sc.x,sc.y); }   // counter-clockwise
