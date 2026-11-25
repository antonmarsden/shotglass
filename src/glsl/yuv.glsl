// The engine's YUV pass (export, #116): the final RGBA picture to 4:2:0 Y'CbCr,
// BT.709 matrix, limited range, chroma sited left (co-sited horizontally with the
// even luma columns, between the two rows vertically). Contract, not looks: it
// decides what a WebM of a frame *is*, like the copy pass decides what the frame is.
//
// One integer target holds the whole I420 picture, laid out as the encoder reads it:
// rows 0 .. h-1 are Y (w samples), rows h .. h + h/2 - 1 hold a row of Cb (w/2
// samples) followed by the matching row of Cr. Target row r is image row r (top
// first), so a readback needs no flip.
uniform sampler2D sg_src;
uniform ivec2 sg_size;   // w, h of the picture (both even)
uniform bool sg_flipped; // sg_src row 0 is the image's top row
uniform float sg_scale;  // 2^(bits - 8)
uniform float sg_max;    // 2^bits - 1
out uint sg_out;

vec3 sg_rgb(ivec2 p) {
  p = clamp(p, ivec2(0), sg_size - 1);
  int y = sg_flipped ? p.y : sg_size.y - 1 - p.y;
  return texelFetch(sg_src, ivec2(p.x, y), 0).rgb;
}

const vec3 SG_KY = vec3(0.2126, 0.7152, 0.0722);

uint sg_quantise(float v) {
  return uint(clamp(floor(v * sg_scale + 0.5), 0.0, sg_max));
}

void main() {
  ivec2 f = ivec2(gl_FragCoord.xy);
  if (f.y < sg_size.y) {
    sg_out = sg_quantise(16.0 + 219.0 * dot(SG_KY, sg_rgb(f)));
    return;
  }
  int half_w = sg_size.x / 2;
  bool cr = f.x >= half_w;
  int x = 2 * (cr ? f.x - half_w : f.x);
  int y = 2 * (f.y - sg_size.y);
  // [1 2 1] across the three columns around the sample, averaged over two rows.
  vec3 c = vec3(0.0);
  for (int dy = 0; dy < 2; dy++) {
    c += sg_rgb(ivec2(x - 1, y + dy)) + 2.0 * sg_rgb(ivec2(x, y + dy)) + sg_rgb(ivec2(x + 1, y + dy));
  }
  c *= 0.125;
  float luma = dot(SG_KY, c);
  float d = cr ? (c.r - luma) / 1.5748 : (c.b - luma) / 1.8556;
  sg_out = sg_quantise(128.0 + 224.0 * d);
}
