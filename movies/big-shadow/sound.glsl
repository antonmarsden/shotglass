// ============================================================
//  BIG SHADOW — soundtrack, synthesized on the GPU.
//  mainSound returns the stereo sample at `time` seconds of the *movie*.
//  CUE_<SHOT>_START / _SETTLED / _END come from movie.toml, so if you retime
//  a shot the music follows on the next reload.
// ============================================================
#define TAU 6.28318530718

uint ihash(uint x){ x ^= x >> 16; x *= 0x7feb352du; x ^= x >> 15; x *= 0x846ca68bu; x ^= x >> 16; return x; }
float rnd(uint x){ return float(ihash(x)) * (1.0 / 4294967295.0); }
float rnd(int x){ return rnd(uint(x)); }

// fades a section in over [a0,a1] and out over [b0,b1]
float window(float t, float a0, float a1, float b0, float b1){
  return smoothstep(a0, a1, t) * (1.0 - smoothstep(b0, b1, t));
}

// pentatonic scale (semitones above A)
float penta(int i){
  int o = i / 5, k = i - o * 5;
  int s = k == 0 ? 0 : k == 1 ? 3 : k == 2 ? 5 : k == 3 ? 7 : 10;
  return float(o * 12 + s);
}
float note(float semis, float base){ return base * exp2(semis / 12.0); }

// ---------- candle crackle: sparse little clicks ----------
float crackle(int samp, float t){
  float v = 0.0;
  const float RATE = 70.0;
  int cell = int(t * RATE);
  for (int c = cell - 2; c <= cell; c++){
    if (rnd(c * 3 + 1) < 0.18){
      float t0 = (float(c) + rnd(c * 3 + 2)) / RATE;
      float dt = t - t0;
      if (dt > 0.0) v += (rnd(samp) * 2.0 - 1.0) * exp(-dt * 380.0) * (0.3 + 0.7 * rnd(c * 3 + 3));
    }
  }
  return v;
}

// ---------- warm drone ----------
float drone(float t, float f){
  float s = 0.0;
  for (int h = 1; h <= 5; h++){
    float fh = f * float(h);
    s += sin(TAU * fh * t + 0.3 * sin(TAU * 0.13 * t * float(h))) / float(h * h);
  }
  return s;
}

// ---------- bells: struck notes with inharmonic partials ----------
vec2 bells(float t, float t0, float step, int seed){
  vec2 s = vec2(0.0);
  float local = t - t0;
  if (local < 0.0) return s;
  int n = int(local / step);
  for (int k = 0; k <= 4; k++){
    int i = n - k;
    if (i < 0) break;
    if (rnd(i * 7 + seed) < 0.25) continue;                 // rests
    float dt = local - float(i) * step;
    float f = note(penta(5 + int(rnd(i * 7 + seed + 1) * 8.0)), 220.0);
    float env = exp(-dt * 1.6) * smoothstep(0.0, 0.004, dt);
    float b = sin(TAU * f * dt) + 0.35 * sin(TAU * f * 2.76 * dt) * exp(-dt * 3.0)
            + 0.15 * sin(TAU * f * 5.40 * dt) * exp(-dt * 6.0);
    float pan = rnd(i * 7 + seed + 2);
    s += b * env * vec2(1.0 - 0.6 * pan, 0.4 + 0.6 * pan);
  }
  return s;
}

// ---------- pizzicato steps for the close-up ----------
vec2 pizz(float t, float t0, float step){
  vec2 s = vec2(0.0);
  float local = t - t0;
  if (local < 0.0) return s;
  int n = int(local / step);
  for (int k = 0; k <= 2; k++){
    int i = n - k;
    if (i < 0) break;
    if ((i & 3) == 3 && rnd(i + 99) < 0.6) continue;
    float dt = local - float(i) * step;
    float f = note(penta(int(rnd(i * 11 + 5) * 7.0)), 110.0);
    float env = exp(-dt * 9.0) * smoothstep(0.0, 0.002, dt);
    // a plucked string: a few harmonics that die off quickly
    float p = sin(TAU * f * dt) + 0.5 * sin(TAU * 2.0 * f * dt) * exp(-dt * 8.0) + 0.25 * sin(TAU * 3.0 * f * dt) * exp(-dt * 14.0);
    float pan = 0.5 + 0.35 * sin(float(i) * 1.7);
    s += p * env * vec2(1.0 - pan, pan) * 1.6;
  }
  return s;
}

vec2 mainSound(int samp, float time){
  float t = time;
  vec2 o = vec2(0.0);

  // 1. wall: crackle + a low drone that swells as the shadow grows,
  //    and a thin uneasy tone on top. All of it dies with the flame.
  float flameOut = 1.0 - smoothstep(CUE_WALL_START + 6.4, CUE_WALL_START + 8.2, t);
  float wall = window(t, CUE_WALL_START, CUE_WALL_START + 1.5, CUE_MOON_START, CUE_MOON_SETTLED);
  float grow = smoothstep(CUE_WALL_START, CUE_WALL_START + 6.0, t);
  o += vec2(crackle(samp, t)) * 0.10 * wall * flameOut;
  float dr = drone(t, 55.0) * (0.10 + 0.12 * grow);
  o += vec2(dr, dr * 0.95) * wall * mix(1.0, 0.4, 1.0 - flameOut);
  float uneasy = sin(TAU * 440.0 * t) + sin(TAU * 443.0 * t);
  o += vec2(0.8, 1.0) * uneasy * 0.012 * grow * grow * wall * flameOut;

  // 2. moon: slow bells over a soft fifth
  float moon = window(t, CUE_MOON_START, CUE_MOON_SETTLED + 0.6, CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED + 0.5);
  o += bells(t, CUE_MOON_START + 0.4, 0.62, 17) * 0.11 * moon;
  float pad = drone(t, 73.42) * 0.06 + drone(t, 110.0) * 0.04;
  o += vec2(pad * 0.9, pad) * moon;

  // 3. close-up: playful plucks, the drone returns warmer
  float close = window(t, CUE_CLOSEUP_START, CUE_CLOSEUP_SETTLED, CUE_END_START, CUE_END_SETTLED);
  o += pizz(t, CUE_CLOSEUP_START + 0.2, 0.3) * 0.09 * close;
  float warm = drone(t, 55.0) * 0.08;
  o += vec2(warm) * close;

  // 4. end: one low thump as the iris shuts, with a long tail
  float dt = t - CUE_END_SETTLED;
  if (dt > 0.0){
    // pitch falls from 112 Hz to 42 Hz: phase is the integral of 42 + 70·e^(-6t)
    float phase = TAU * (42.0 * dt + 70.0 / 6.0 * (1.0 - exp(-dt * 6.0)));
    o += vec2(sin(phase)) * exp(-dt * 2.6) * 0.45 * smoothstep(0.0, 0.005, dt);
  }

  // master: gentle fade at the very start and end, soft clip
  o *= smoothstep(0.0, 0.3, t) * (1.0 - smoothstep(MOVIE_DURATION - 0.4, MOVIE_DURATION, t));
  return tanh(o * 1.2) * 0.9;
}
