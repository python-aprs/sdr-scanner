/*
 * The interface between sdr-scanner (Rust) and shim.c. Keep in sync with
 * crates/sources/src/sdrplay/ffi.rs.
 */
#ifndef SCANNER_SDRPLAY_H
#define SCANNER_SDRPLAY_H

typedef struct {
    char serial[64];
    char model[32];
    int hw_ver;
} scn_sdrplay_device;

typedef struct {
    /* Empty: the first device found. */
    char serial[64];
    double sample_rate;
    double center_hz;
    int bandwidth_khz;
    /* LNA state (0 is the most gain) and IF gain reduction in dB (20-59). */
    int lna_state;
    int gr_db;
    int agc;
    int agc_setpoint_dbfs;
    double ppm;
    int bias_t;
    int rf_notch;
    int dab_notch;
    /* USB transfer mode: 0 isochronous (the API default), 1 bulk. */
    int usb_bulk;
} scn_sdrplay_config;

enum {
    SCN_EVENT_GAIN = 1,
    SCN_EVENT_OVERLOAD = 2,
    SCN_EVENT_OVERLOAD_CLEARED = 3,
    SCN_EVENT_REMOVED = 4,
    SCN_EVENT_FAILURE = 5,
};

typedef void (*scn_samples_fn)(void *ctx, const short *xi, const short *xq,
                               unsigned int n, unsigned int first_sample, int reset);
/* For SCN_EVENT_GAIN: a = IF gain reduction dB, b = LNA gain reduction dB,
 * gain = current system gain dB. */
typedef void (*scn_event_fn)(void *ctx, int kind, int a, int b, double gain);

typedef struct scn_sdrplay scn_sdrplay;

/* Returns the number of devices (may exceed max), or -1 with err set. */
int scn_sdrplay_list(scn_sdrplay_device *out, int max, char *err, int errlen);

/* Open, configure and start streaming. Callbacks run on API threads until
 * scn_sdrplay_stop returns. Returns 0, or -1 with err set. */
int scn_sdrplay_start(const scn_sdrplay_config *cfg, scn_samples_fn on_samples,
                      scn_event_fn on_event, void *ctx, scn_sdrplay **out,
                      scn_sdrplay_device *chosen, char *err, int errlen);

int scn_sdrplay_set_gain(scn_sdrplay *h, int lna_state, int gr_db, char *err, int errlen);

/* Stop streaming and release the device. No callbacks run after it returns. */
void scn_sdrplay_stop(scn_sdrplay *h);

#endif
