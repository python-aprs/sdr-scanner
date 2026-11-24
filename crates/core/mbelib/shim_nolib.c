/* Stand-in when mbelib.h was not found at build time: no vocoder. */
#include <stddef.h>
void *scanner_mbe_new(void) { return NULL; }
void scanner_mbe_free(void *p) { (void)p; }
void scanner_mbe_decode(void *p, const char *ambe_d, int errs, float *out) {
    (void)p; (void)ambe_d; (void)errs;
    for (int i = 0; i < 160; i++) out[i] = 0.0f;
}
