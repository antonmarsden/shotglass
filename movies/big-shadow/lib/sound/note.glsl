// Big Shadow, lib/sound: note.glsl
float bs_note_hz(float note) { return 440.0 * exp2((note - 69.0) / 12.0); }
float bs_note_hz(int note) { return bs_note_hz(float(note)); }

int bs_scale_note(int degree, int root, int mask) {
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

int bs_scale_minor_pentatonic(int degree, int root) { return bs_scale_note(degree, root, 0x4A9); } // 0 3 5 7 10
