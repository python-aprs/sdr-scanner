/*
 * Thin C layer between sdr-scanner and the SDRplay API 3.x.
 *
 * The API's structs are large and change between releases, so instead of
 * mirroring them in Rust we compile this file against the installed
 * sdrplay_api.h and give Rust a small, stable interface (scanner_sdrplay.h).
 *
 * The API library is opened at run time with dlopen, so the scanner binary
 * builds with only the header present and starts (and reports a clear error)
 * on machines without the API installed. Set SDRPLAY_API_LIB to the library's
 * path if it is somewhere unusual.
 */

#include "scanner_sdrplay.h"

#include <dlfcn.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <sdrplay_api.h>

/* Function table, filled by dlsym. */
static struct {
    void *lib;
    __typeof__(sdrplay_api_Open) *Open;
    __typeof__(sdrplay_api_Close) *Close;
    __typeof__(sdrplay_api_ApiVersion) *ApiVersion;
    __typeof__(sdrplay_api_LockDeviceApi) *LockDeviceApi;
    __typeof__(sdrplay_api_UnlockDeviceApi) *UnlockDeviceApi;
    __typeof__(sdrplay_api_GetDevices) *GetDevices;
    __typeof__(sdrplay_api_SelectDevice) *SelectDevice;
    __typeof__(sdrplay_api_ReleaseDevice) *ReleaseDevice;
    __typeof__(sdrplay_api_GetErrorString) *GetErrorString;
    __typeof__(sdrplay_api_GetDeviceParams) *GetDeviceParams;
    __typeof__(sdrplay_api_Init) *Init;
    __typeof__(sdrplay_api_Uninit) *Uninit;
    __typeof__(sdrplay_api_Update) *Update;
} api;

static void set_err(char *err, int errlen, const char *fmt, ...) {
    if (!err || errlen <= 0)
        return;
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(err, (size_t)errlen, fmt, ap);
    va_end(ap);
}

static const char *err_str(sdrplay_api_ErrT e) {
    const char *s = api.GetErrorString ? api.GetErrorString(e) : NULL;
    return s ? s : "unknown error";
}

static int load_api(char *err, int errlen) {
    if (api.lib)
        return 0;
    const char *candidates[] = {
        getenv("SDRPLAY_API_LIB"),
#ifdef __APPLE__
        "/usr/local/lib/libsdrplay_api.so.3",
        "/usr/local/lib/libsdrplay_api.so",
        "/usr/local/lib/libsdrplay_api.so.3.15",
        "/usr/local/lib/libsdrplay_api.so.3.14",
        "/opt/homebrew/lib/libsdrplay_api.so.3",
        "libsdrplay_api.so.3",
        "libsdrplay_api.dylib",
#else
        "libsdrplay_api.so.3",
        "/usr/local/lib/libsdrplay_api.so.3",
        "libsdrplay_api.so",
#endif
    };
    const char *last = "no candidates";
    for (size_t i = 0; i < sizeof candidates / sizeof candidates[0]; i++) {
        if (!candidates[i] || !*candidates[i])
            continue;
        api.lib = dlopen(candidates[i], RTLD_NOW | RTLD_LOCAL);
        if (api.lib)
            break;
        last = dlerror();
    }
    if (!api.lib) {
        set_err(err, errlen,
                "SDRplay API library not found (%s). Install SDRplay API 3.x "
                "or set SDRPLAY_API_LIB to the library path",
                last ? last : "?");
        return -1;
    }
#define LOAD(name)                                                             \
    do {                                                                       \
        *(void **)(&api.name) = dlsym(api.lib, "sdrplay_api_" #name);          \
        if (!api.name) {                                                       \
            set_err(err, errlen, "SDRplay API library lacks sdrplay_api_" #name); \
            dlclose(api.lib);                                                  \
            memset(&api, 0, sizeof api);                                       \
            return -1;                                                         \
        }                                                                      \
    } while (0)
    LOAD(Open);
    LOAD(Close);
    LOAD(ApiVersion);
    LOAD(LockDeviceApi);
    LOAD(UnlockDeviceApi);
    LOAD(GetDevices);
    LOAD(SelectDevice);
    LOAD(ReleaseDevice);
    LOAD(GetErrorString);
    LOAD(GetDeviceParams);
    LOAD(Init);
    LOAD(Uninit);
    LOAD(Update);
#undef LOAD
    return 0;
}

/* Open the API and check the library matches the header we built against. */
static int open_api(char *err, int errlen) {
    if (load_api(err, errlen))
        return -1;
    sdrplay_api_ErrT e = api.Open();
    if (e != sdrplay_api_Success) {
        set_err(err, errlen,
                "sdrplay_api_Open: %s (is the sdrplay_apiService daemon running?)",
                err_str(e));
        return -1;
    }
    float ver = 0;
    e = api.ApiVersion(&ver);
    if (e != sdrplay_api_Success) {
        set_err(err, errlen, "sdrplay_api_ApiVersion: %s", err_str(e));
        api.Close();
        return -1;
    }
    if (floorf(ver) != floorf(SDRPLAY_API_VERSION)) {
        set_err(err, errlen,
                "SDRplay API library is version %.2f but scanner was built "
                "against %.2f",
                ver, (double)SDRPLAY_API_VERSION);
        api.Close();
        return -1;
    }
    return 0;
}

static const char *hw_name(unsigned char hw) {
    switch (hw) {
    case SDRPLAY_RSP1_ID: return "RSP1";
    case SDRPLAY_RSP1A_ID: return "RSP1A";
#ifdef SDRPLAY_RSP1B_ID
    case SDRPLAY_RSP1B_ID: return "RSP1B";
#endif
    case SDRPLAY_RSP2_ID: return "RSP2";
    case SDRPLAY_RSPduo_ID: return "RSPduo";
    case SDRPLAY_RSPdx_ID: return "RSPdx";
#ifdef SDRPLAY_RSPdxR2_ID
    case SDRPLAY_RSPdxR2_ID: return "RSPdx-R2";
#endif
    default: return "unknown RSP";
    }
}

int scn_sdrplay_list(scn_sdrplay_device *out, int max, char *err, int errlen) {
    if (open_api(err, errlen))
        return -1;
    sdrplay_api_DeviceT devs[SDRPLAY_MAX_DEVICES];
    unsigned int n = 0;
    api.LockDeviceApi();
    sdrplay_api_ErrT e = api.GetDevices(devs, &n, SDRPLAY_MAX_DEVICES);
    api.UnlockDeviceApi();
    if (e != sdrplay_api_Success) {
        set_err(err, errlen, "sdrplay_api_GetDevices: %s", err_str(e));
        api.Close();
        return -1;
    }
    int k = 0;
    for (unsigned int i = 0; i < n && k < max; i++, k++) {
        memset(&out[k], 0, sizeof out[k]);
        snprintf(out[k].serial, sizeof out[k].serial, "%s", devs[i].SerNo);
        snprintf(out[k].model, sizeof out[k].model, "%s", hw_name(devs[i].hwVer));
        out[k].hw_ver = devs[i].hwVer;
    }
    api.Close();
    return (int)n;
}

struct scn_sdrplay {
    sdrplay_api_DeviceT dev;
    sdrplay_api_DeviceParamsT *params;
    scn_samples_fn on_samples;
    scn_event_fn on_event;
    void *ctx;
    int streaming;
};

static void stream_a(short *xi, short *xq, sdrplay_api_StreamCbParamsT *p,
                     unsigned int n, unsigned int reset, void *ctx) {
    struct scn_sdrplay *h = ctx;
    h->on_samples(h->ctx, xi, xq, n, p ? p->firstSampleNum : 0, (int)reset);
}

static void stream_b(short *xi, short *xq, sdrplay_api_StreamCbParamsT *p,
                     unsigned int n, unsigned int reset, void *ctx) {
    (void)xi; (void)xq; (void)p; (void)n; (void)reset; (void)ctx;
}

static void event_cb(sdrplay_api_EventT id, sdrplay_api_TunerSelectT tuner,
                     sdrplay_api_EventParamsT *p, void *ctx) {
    struct scn_sdrplay *h = ctx;
    switch (id) {
    case sdrplay_api_GainChange:
        h->on_event(h->ctx, SCN_EVENT_GAIN, (int)p->gainParams.gRdB,
                    (int)p->gainParams.lnaGRdB, p->gainParams.currGain);
        break;
    case sdrplay_api_PowerOverloadChange: {
        int detected = p->powerOverloadParams.powerOverloadChangeType ==
                       sdrplay_api_Overload_Detected;
        /* The API stops reporting until each message is acknowledged. */
        api.Update(h->dev.dev, tuner, sdrplay_api_Update_Ctrl_OverloadMsgAck,
                   sdrplay_api_Update_Ext1_None);
        h->on_event(h->ctx, detected ? SCN_EVENT_OVERLOAD : SCN_EVENT_OVERLOAD_CLEARED,
                    0, 0, 0.0);
        break;
    }
    case sdrplay_api_DeviceRemoved:
        h->on_event(h->ctx, SCN_EVENT_REMOVED, 0, 0, 0.0);
        break;
    case sdrplay_api_DeviceFailure:
        h->on_event(h->ctx, SCN_EVENT_FAILURE, 0, 0, 0.0);
        break;
    default:
        break;
    }
}

static sdrplay_api_Bw_MHzT bw_type(int khz) {
    if (khz >= 8000) return sdrplay_api_BW_8_000;
    if (khz >= 7000) return sdrplay_api_BW_7_000;
    if (khz >= 6000) return sdrplay_api_BW_6_000;
    if (khz >= 5000) return sdrplay_api_BW_5_000;
    if (khz >= 1536) return sdrplay_api_BW_1_536;
    if (khz >= 600) return sdrplay_api_BW_0_600;
    if (khz >= 300) return sdrplay_api_BW_0_300;
    return sdrplay_api_BW_0_200;
}

int scn_sdrplay_start(const scn_sdrplay_config *cfg, scn_samples_fn on_samples,
                      scn_event_fn on_event, void *ctx, scn_sdrplay **out,
                      scn_sdrplay_device *chosen, char *err, int errlen) {
    *out = NULL;
    if (open_api(err, errlen))
        return -1;
    struct scn_sdrplay *h = calloc(1, sizeof *h);
    if (!h) {
        set_err(err, errlen, "out of memory");
        api.Close();
        return -1;
    }
    h->on_samples = on_samples;
    h->on_event = on_event;
    h->ctx = ctx;

    sdrplay_api_DeviceT devs[SDRPLAY_MAX_DEVICES];
    unsigned int n = 0;
    api.LockDeviceApi();
    sdrplay_api_ErrT e = api.GetDevices(devs, &n, SDRPLAY_MAX_DEVICES);
    if (e != sdrplay_api_Success) {
        api.UnlockDeviceApi();
        set_err(err, errlen, "sdrplay_api_GetDevices: %s", err_str(e));
        goto fail_api;
    }
    int pick = -1;
    for (unsigned int i = 0; i < n; i++) {
        if (cfg->serial[0] == 0 || strcmp(cfg->serial, devs[i].SerNo) == 0) {
            pick = (int)i;
            break;
        }
    }
    if (pick < 0) {
        api.UnlockDeviceApi();
        if (n == 0)
            set_err(err, errlen, "no SDRplay device found (is it plugged in?)");
        else
            set_err(err, errlen, "no SDRplay device with serial %s (found %u)",
                    cfg->serial, n);
        goto fail_api;
    }
    h->dev = devs[pick];
    if (h->dev.hwVer == SDRPLAY_RSPduo_ID) {
        h->dev.tuner = sdrplay_api_Tuner_A;
        h->dev.rspDuoMode = sdrplay_api_RspDuoMode_Single_Tuner;
    }
    e = api.SelectDevice(&h->dev);
    api.UnlockDeviceApi();
    if (e != sdrplay_api_Success) {
        set_err(err, errlen, "sdrplay_api_SelectDevice: %s (in use by another program?)",
                err_str(e));
        goto fail_api;
    }
    if (chosen) {
        memset(chosen, 0, sizeof *chosen);
        snprintf(chosen->serial, sizeof chosen->serial, "%s", h->dev.SerNo);
        snprintf(chosen->model, sizeof chosen->model, "%s", hw_name(h->dev.hwVer));
        chosen->hw_ver = h->dev.hwVer;
    }

    e = api.GetDeviceParams(h->dev.dev, &h->params);
    if (e != sdrplay_api_Success || !h->params || !h->params->devParams ||
        !h->params->rxChannelA) {
        set_err(err, errlen, "sdrplay_api_GetDeviceParams: %s", err_str(e));
        goto fail_dev;
    }
    sdrplay_api_DevParamsT *dp = h->params->devParams;
    sdrplay_api_RxChannelParamsT *rx = h->params->rxChannelA;

    dp->fsFreq.fsHz = cfg->sample_rate;
    dp->mode = cfg->usb_bulk ? sdrplay_api_BULK : sdrplay_api_ISOCH;
    dp->ppm = cfg->ppm;
    rx->tunerParams.rfFreq.rfHz = cfg->center_hz;
    rx->tunerParams.bwType = bw_type(cfg->bandwidth_khz);
    rx->tunerParams.ifType = sdrplay_api_IF_Zero;
    rx->tunerParams.loMode = sdrplay_api_LO_Auto;
    rx->tunerParams.gain.LNAstate = (unsigned char)cfg->lna_state;
    rx->tunerParams.gain.gRdB = cfg->gr_db;
    rx->tunerParams.gain.minGr = sdrplay_api_NORMAL_MIN_GR;
    rx->ctrlParams.agc.enable = cfg->agc ? sdrplay_api_AGC_CTRL_EN : sdrplay_api_AGC_DISABLE;
    if (cfg->agc)
        rx->ctrlParams.agc.setPoint_dBfs = cfg->agc_setpoint_dbfs;
    rx->ctrlParams.decimation.enable = 0;
    rx->ctrlParams.decimation.decimationFactor = 1;
    rx->ctrlParams.dcOffset.DCenable = 1;
    rx->ctrlParams.dcOffset.IQenable = 1;
    if (h->dev.hwVer == SDRPLAY_RSP1A_ID
#ifdef SDRPLAY_RSP1B_ID
        || h->dev.hwVer == SDRPLAY_RSP1B_ID
#endif
    ) {
        dp->rsp1aParams.rfNotchEnable = (unsigned char)cfg->rf_notch;
        dp->rsp1aParams.rfDabNotchEnable = (unsigned char)cfg->dab_notch;
        rx->rsp1aTunerParams.biasTEnable = (unsigned char)cfg->bias_t;
    } else if (cfg->bias_t || cfg->rf_notch || cfg->dab_notch) {
        set_err(err, errlen, "bias-T and notch options are only wired up for the RSP1A/RSP1B");
        goto fail_dev;
    }

    sdrplay_api_CallbackFnsT cbs = {stream_a, stream_b, event_cb};
    e = api.Init(h->dev.dev, &cbs, h);
    if (e != sdrplay_api_Success) {
        set_err(err, errlen, "sdrplay_api_Init: %s", err_str(e));
        goto fail_dev;
    }
    h->streaming = 1;
    *out = h;
    return 0;

fail_dev:
    api.LockDeviceApi();
    api.ReleaseDevice(&h->dev);
    api.UnlockDeviceApi();
fail_api:
    api.Close();
    free(h);
    return -1;
}

int scn_sdrplay_set_gain(scn_sdrplay *h, int lna_state, int gr_db, char *err, int errlen) {
    sdrplay_api_RxChannelParamsT *rx = h->params->rxChannelA;
    rx->tunerParams.gain.LNAstate = (unsigned char)lna_state;
    rx->tunerParams.gain.gRdB = gr_db;
    sdrplay_api_ErrT e = api.Update(h->dev.dev, h->dev.tuner, sdrplay_api_Update_Tuner_Gr,
                                    sdrplay_api_Update_Ext1_None);
    if (e != sdrplay_api_Success) {
        set_err(err, errlen, "sdrplay_api_Update (gain): %s", err_str(e));
        return -1;
    }
    return 0;
}

void scn_sdrplay_stop(scn_sdrplay *h) {
    if (!h)
        return;
    if (h->streaming)
        api.Uninit(h->dev.dev);
    api.LockDeviceApi();
    api.ReleaseDevice(&h->dev);
    api.UnlockDeviceApi();
    api.Close();
    free(h);
}
