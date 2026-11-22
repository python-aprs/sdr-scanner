/*
 * Stand-in for shim.c, compiled when the sdrplay feature is on but the
 * SDRplay API header was not found at build time (CI, or a machine without
 * the SDK). It keeps the Rust side building and fails at run time with a
 * clear message instead of guessing the vendor's struct layouts.
 */
#include "scanner_sdrplay.h"

#include <stdio.h>

static const char MSG[] =
    "this scanner was built without the SDRplay API header (sdrplay_api.h); "
    "install SDRplay API 3.x and rebuild, or set SDRPLAY_API_INCLUDE";

int scn_sdrplay_list(scn_sdrplay_device *out, int max, char *err, int errlen) {
    (void)out; (void)max;
    if (err && errlen > 0)
        snprintf(err, (size_t)errlen, "%s", MSG);
    return -1;
}

int scn_sdrplay_start(const scn_sdrplay_config *cfg, scn_samples_fn on_samples,
                      scn_event_fn on_event, void *ctx, scn_sdrplay **out,
                      scn_sdrplay_device *chosen, char *err, int errlen) {
    (void)cfg; (void)on_samples; (void)on_event; (void)ctx; (void)chosen;
    *out = NULL;
    if (err && errlen > 0)
        snprintf(err, (size_t)errlen, "%s", MSG);
    return -1;
}

int scn_sdrplay_set_gain(scn_sdrplay *h, int lna_state, int gr_db, char *err, int errlen) {
    (void)h; (void)lna_state; (void)gr_db;
    if (err && errlen > 0)
        snprintf(err, (size_t)errlen, "%s", MSG);
    return -1;
}

void scn_sdrplay_stop(scn_sdrplay *h) { (void)h; }
