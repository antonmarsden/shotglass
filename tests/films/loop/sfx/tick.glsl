// A tick every half second of the shot; uPitch tells the shots apart.
vec2 mainSound(int samp, float time) {
  float t = fract(max(time, 0.0) * 2.0) * 0.5;
  float s = exp(-14.0 * t) * sin(6.2831853 * 660.0 * uPitch * t);
  return vec2(0.25 * s, 0.2 * s);
}
