// A tiny C layer over SVT-AV1's encoder API, so the Rust side needs no bindgen and
// never sees SVT's configuration struct (whose layout changes between releases).
// Settings go through svt_av1_enc_parse_parameter by name, as `SvtAv1EncApp` and
// ffmpeg's -svtav1-params do.

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "EbSvtAv1Enc.h"

typedef struct SgSvt {
    EbComponentType* handle;
    EbSvtAv1EncConfiguration config;
    EbBufferHeaderType input;
    EbSvtIOFormat planes;
    EbBufferHeaderType* packet;
    int depth;
    int eos;
} SgSvt;

// Errors only: SVT's banner and per-encoder notes would bury the export's progress.
static void sg_svt_log(void* ctx, SvtAv1LogLevel level, const char* tag, const char* fmt, va_list args) {
    (void)ctx;
    if (level > SVT_AV1_LOG_ERROR) return;
    fprintf(stderr, "%s: ", tag ? tag : "SVT-AV1");
    vfprintf(stderr, fmt, args);
}

// Create an encoder with default settings. 0 on success.
int sg_svt_new(SgSvt** out) {
    svt_av1_set_log_callback(sg_svt_log, NULL);
    SgSvt* s = (SgSvt*)calloc(1, sizeof(SgSvt));
    if (!s) return -1;
    if (svt_av1_enc_init_handle(&s->handle, &s->config) != EB_ErrorNone) {
        free(s);
        return -2;
    }
    *out = s;
    return 0;
}

// Set one parameter by name (SvtAv1EncApp's long option names). 0 on success.
int sg_svt_set(SgSvt* s, const char* name, const char* value) {
    if (strcmp(name, "input-depth") == 0) s->depth = atoi(value);
    return svt_av1_enc_parse_parameter(&s->config, name, value) == EB_ErrorNone ? 0 : -1;
}

// Apply the parameters and allocate the encoder. 0 on success.
int sg_svt_init(SgSvt* s) {
    if (svt_av1_enc_set_parameter(s->handle, &s->config) != EB_ErrorNone) return -1;
    if (svt_av1_enc_init(s->handle) != EB_ErrorNone) return -2;
    return 0;
}

// The sequence header OBU(s), copied into buf (up to cap bytes). Returns the length,
// or a negative number on failure.
int sg_svt_header(SgSvt* s, uint8_t* buf, uint32_t cap) {
    EbBufferHeaderType* h = NULL;
    if (svt_av1_enc_stream_header(s->handle, &h) != EB_ErrorNone || !h) return -1;
    uint32_t n = h->n_filled_len;
    int ret = n <= cap ? (int)n : -2;
    if (ret > 0) memcpy(buf, h->p_buffer, n);
    svt_av1_enc_stream_header_release(h);
    return ret;
}

// Send one picture: planes with strides in samples (not bytes). 0 on success.
int sg_svt_send(SgSvt* s, const uint8_t* y, const uint8_t* cb, const uint8_t* cr, uint32_t y_stride,
                uint32_t c_stride, int64_t pts, uint32_t bytes) {
    memset(&s->input, 0, sizeof(s->input));
    s->planes.luma = (uint8_t*)y;
    s->planes.cb = (uint8_t*)cb;
    s->planes.cr = (uint8_t*)cr;
    s->planes.y_stride = y_stride;
    s->planes.cb_stride = c_stride;
    s->planes.cr_stride = c_stride;
    s->input.size = sizeof(EbBufferHeaderType);
    s->input.p_buffer = (uint8_t*)&s->planes;
    s->input.n_filled_len = bytes;
    s->input.n_alloc_len = bytes;
    s->input.pts = pts;
    s->input.pic_type = EB_AV1_INVALID_PICTURE;
    return svt_av1_enc_send_picture(s->handle, &s->input) == EB_ErrorNone ? 0 : -1;
}

// Signal the end of the input. 0 on success.
int sg_svt_send_eos(SgSvt* s) {
    EbBufferHeaderType eos;
    memset(&eos, 0, sizeof(eos));
    eos.size = sizeof(EbBufferHeaderType);
    eos.flags = EB_BUFFERFLAG_EOS;
    eos.pic_type = EB_AV1_INVALID_PICTURE;
    return svt_av1_enc_send_picture(s->handle, &eos) == EB_ErrorNone ? 0 : -1;
}

// The next packet. 0: a packet (valid until the next call or sg_svt_free);
// 1: none ready yet (only when !done); 2: the end of the stream; < 0: an error.
int sg_svt_receive(SgSvt* s, int done, const uint8_t** data, uint32_t* len, int64_t* pts,
                   int* key) {
    if (s->packet) svt_av1_enc_release_out_buffer(&s->packet);
    s->packet = NULL;
    if (s->eos) return 2;
    EbBufferHeaderType* p = NULL;
    EbErrorType e = svt_av1_enc_get_packet(s->handle, &p, (uint8_t)(done != 0));
    if (e == EB_NoErrorEmptyQueue) return 1;
    if (e != EB_ErrorNone || !p) return -1;
    if (p->flags & EB_BUFFERFLAG_ERROR_MASK) {
        svt_av1_enc_release_out_buffer(&p);
        return -2;
    }
    int eos = (p->flags & EB_BUFFERFLAG_EOS) != 0;
    if (eos && p->n_filled_len == 0) {
        svt_av1_enc_release_out_buffer(&p);
        return 2;
    }
    s->eos = eos;
    s->packet = p;
    *data = p->p_buffer;
    *len = p->n_filled_len;
    *pts = p->pts;
    *key = p->pic_type == EB_AV1_KEY_PICTURE;
    return 0;
}

const char* sg_svt_version(void) { return svt_av1_get_version(); }

void sg_svt_free(SgSvt* s) {
    if (!s) return;
    if (s->packet) svt_av1_enc_release_out_buffer(&s->packet);
    svt_av1_enc_deinit(s->handle);
    svt_av1_enc_deinit_handle(s->handle);
    free(s);
}
