// Shared audio helpers.
#define TAU 6.28318530718

uint ihash(uint x){ x ^= x >> 16; x *= 0x7feb352du; x ^= x >> 15; x *= 0x846ca68bu; x ^= x >> 16; return x; }
float rnd(uint x){ return float(ihash(x)) * (1.0 / 4294967295.0); }
float rnd(int x){ return rnd(uint(x)); }

// pentatonic scale (semitones above the root)
float penta(int i){
  int o = i / 5, k = i - o * 5;
  int s = k == 0 ? 0 : k == 1 ? 3 : k == 2 ? 5 : k == 3 ? 7 : 10;
  return float(o * 12 + s);
}
float note(float semis, float base){ return base * exp2(semis / 12.0); }

// a warm drone: a few soft harmonics with slow vibrato
float drone(float t, float f){
  float s = 0.0;
  for (int h = 1; h <= 5; h++){
    float fh = f * float(h);
    s += sin(TAU * fh * t + 0.3 * sin(TAU * 0.13 * t * float(h))) / float(h * h);
  }
  return s;
}
