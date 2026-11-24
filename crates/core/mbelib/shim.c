/* Thin wrapper over mbelib-neo, or classic mbelib, for the scanner's
 * `mbelib` feature: hides the parameter layout and the two libraries'
 * different decode calls from Rust. */
#include <stdlib.h>
#ifdef SCANNER_MBELIB_NEO
#include <mbelib-neo/mbelib.h>
#else
#include <mbelib.h>
#endif

struct scanner_mbe {
    mbe_parms cur, prev, prev_enh;
};

void *scanner_mbe_new(void) {
    struct scanner_mbe *m = calloc(1, sizeof *m);
    if (m) mbe_initMbeParms(&m->cur, &m->prev, &m->prev_enh);
    return m;
}

void scanner_mbe_free(void *p) { free(p); }

/* One 20 ms frame of 49 parameter bits to 160 samples at 8 kHz, scaled like
 * 16-bit audio: AMBE+2 (3600x2450, DMR and System Fusion), or with `dstar`
 * set D-STAR's AMBE (3600x2400). `errs` is the bit errors the channel coding
 * corrected; 10 or more means C0 was past correction. */
void scanner_mbe_decode(void *p, const char *ambe_d, int errs, int dstar, float *out) {
    struct scanner_mbe *m = p;
    char d[49];
    for (int i = 0; i < 49; i++) d[i] = ambe_d[i];
#ifdef SCANNER_MBELIB_NEO
    mbe_process_result r;
    mbe_initProcessResult(&r);
    r.flags |= MBE_PROCESS_FLAG_C0_VALID;
    if (errs >= 10) {
        r.flags |= MBE_PROCESS_FLAG_C0_UNCORRECTABLE;
        r.c0_errors = 4;
    } else {
        r.protected_errors = errs;
    }
    r.total_errors = errs;
    if (dstar)
        mbe_processAmbe2400Dataf(out, &r, d, &m->cur, &m->prev, &m->prev_enh);
    else
        mbe_processAmbe2450Dataf(out, &r, d, &m->cur, &m->prev, &m->prev_enh);
#else
    char err_str[64];
    int e = errs, e2 = errs;
    if (dstar)
        mbe_processAmbe2400Dataf(out, &e, &e2, err_str, d, &m->cur, &m->prev,
                                 &m->prev_enh, 3);
    else
        mbe_processAmbe2450Dataf(out, &e, &e2, err_str, d, &m->cur, &m->prev,
                                 &m->prev_enh, 3);
#endif
}
