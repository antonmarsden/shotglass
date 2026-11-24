// Shared audio helpers. Notes, scales and instruments come from lib/sound,
// random numbers from lib/base's integer hash.
#include <base/noise.glsl>
#include <sound/note.glsl>
#include <sound/instrument.glsl>
#include <sound/drum.glsl>

#define TAU 6.28318530718

// the film's scale: minor pentatonic (0 3 5 7 10) on MIDI note `root` (57 = A3, 220 Hz)
float penta_hz(int degree, int root){ return kit_note_hz(kit_scale_minor_pentatonic(degree, root)); }

// a warm drone: a few soft harmonics with slow vibrato
float drone(float t, float f){
  float s = 0.0;
  for (int h = 1; h <= 5; h++){
    float fh = f * float(h);
    s += sin(TAU * fh * t + 0.3 * sin(TAU * 0.13 * t * float(h))) / float(h * h);
  }
  return s;
}
