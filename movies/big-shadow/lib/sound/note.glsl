// Big Shadow, lib/sound: note.glsl
//
// Pitch: MIDI note numbers, scales and chords. Notes are integers in MIDI numbering
// (60 = middle C, 69 = A 440 Hz, one step per semitone), so melodies are plain
// integer arithmetic and only become hertz at the end, with kit_note_hz.
//
// A scale or chord is a 12-bit mask of the semitones above its root that it uses
// (bit 0 = the root, bit 7 = the fifth): 0xAB5 is major (0 2 4 5 7 9 11), 0x91 a
// major triad (0 4 7). Degrees past the last note carry on into the next octave, and
// negative degrees go below the root, so a degree is all a melody needs.
// sound: yes

// kit_note_hz(note): frequency of a MIDI note, equal temperament, A4 = 440 Hz.
//   note  MIDI note number (60 = middle C); a float allows detuning (60.1 = 10 cents sharp)
//   ->  hertz
//   e.g. float hz = kit_note_hz(57);   // 220.0 (A3)
float kit_note_hz(float note) { return 440.0 * exp2((note - 69.0) / 12.0); }
float kit_note_hz(int note) { return kit_note_hz(float(note)); }

// kit_scale_note(degree, root, mask): the note at a degree of any scale or chord.
//   degree  0 = root, 1 = the next note up, ...; any int (past the end: next octave up,
//           negative: below the root)
//   root    MIDI note of degree 0
//   mask    the scale's semitones, bit i = i semitones above the root; bit 0 must be set
//           (a mask without it is treated as if it were). Bits above 11 are ignored.
//   ->  MIDI note number
//   cost: a loop over 12 bits
//   e.g. int n = kit_scale_note(step, 62, 0x6AD);   // D dorian: 0 2 3 5 7 9 10
int kit_scale_note(int degree, int root, int mask) {
    mask = (mask & 0xFFF) | 1;
    int n = 0;
    for (int i = 0; i < 12; i++) n += (mask >> i) & 1;
    int octave = degree >= 0 ? degree / n : -((n - 1 - degree) / n);
    int r = degree - octave * n;   // 0..n-1
    int semi = 0;
    for (int i = 0; i < 12; i++) {
        if (((mask >> i) & 1) != 0) {
            if (r == 0) { semi = i; break; }
            r--;
        }
    }
    return root + 12 * octave + semi;
}

// kit_scale_minor_pentatonic(degree, root): kit_scale_note with the minor pentatonic mask.
//   degree  0 = root; past the end goes up an octave, negative goes below the root
//   root    MIDI note of degree 0 (the key)
//   ->  MIDI note number
//   e.g. int n = kit_scale_minor_pentatonic(5, 57);   // 69, A an octave up
int kit_scale_minor_pentatonic(int degree, int root) { return kit_scale_note(degree, root, 0x4A9); } // 0 3 5 7 10
