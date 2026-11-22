/* A stand-in for sdrplay_api.h, written from the API 3.x documentation and
 * covering only what shim.c uses. It is NOT the vendor header: build real
 * binaries against the installed SDK. Used with fake_api.c to build and run
 * the sdrplay feature where the SDK is not installed (see run-e2e.sh). */
#define SDRPLAY_API_VERSION (float)(3.15)
#define SDRPLAY_MAX_DEVICES 16
#define SDRPLAY_MAX_SER_NO_LEN 64
#define SDRPLAY_RSP1_ID 1
#define SDRPLAY_RSP1A_ID 255
#define SDRPLAY_RSP2_ID 2
#define SDRPLAY_RSPduo_ID 3
#define SDRPLAY_RSPdx_ID 4
#define SDRPLAY_RSP1B_ID 6
#define SDRPLAY_RSPdxR2_ID 7
typedef void *HANDLE;
typedef enum { sdrplay_api_Success = 0, sdrplay_api_Fail = 1 } sdrplay_api_ErrT;
typedef enum { sdrplay_api_Tuner_Neither=0, sdrplay_api_Tuner_A=1 } sdrplay_api_TunerSelectT;
typedef enum { sdrplay_api_RspDuoMode_Unknown=0, sdrplay_api_RspDuoMode_Single_Tuner=1 } sdrplay_api_RspDuoModeT;
typedef struct { char SerNo[SDRPLAY_MAX_SER_NO_LEN]; unsigned char hwVer; sdrplay_api_TunerSelectT tuner; sdrplay_api_RspDuoModeT rspDuoMode; unsigned char valid; double rspDuoSampleFreq; HANDLE dev; } sdrplay_api_DeviceT;
typedef enum { sdrplay_api_BW_Undefined=0, sdrplay_api_BW_0_200=200, sdrplay_api_BW_0_300=300, sdrplay_api_BW_0_600=600, sdrplay_api_BW_1_536=1536, sdrplay_api_BW_5_000=5000, sdrplay_api_BW_6_000=6000, sdrplay_api_BW_7_000=7000, sdrplay_api_BW_8_000=8000 } sdrplay_api_Bw_MHzT;
typedef enum { sdrplay_api_IF_Undefined=-1, sdrplay_api_IF_Zero=0 } sdrplay_api_If_kHzT;
typedef enum { sdrplay_api_LO_Undefined=0, sdrplay_api_LO_Auto=1 } sdrplay_api_LoModeT;
typedef enum { sdrplay_api_EXTENDED_MIN_GR=0, sdrplay_api_NORMAL_MIN_GR=20 } sdrplay_api_MinGainReductionT;
typedef struct { int gRdB; unsigned char LNAstate; unsigned char syncUpdate; sdrplay_api_MinGainReductionT minGr; } sdrplay_api_GainT;
typedef struct { double rfHz; unsigned char syncUpdate; } sdrplay_api_RfFreqT;
typedef struct { sdrplay_api_Bw_MHzT bwType; sdrplay_api_If_kHzT ifType; sdrplay_api_LoModeT loMode; sdrplay_api_GainT gain; sdrplay_api_RfFreqT rfFreq; } sdrplay_api_TunerParamsT;
typedef enum { sdrplay_api_AGC_DISABLE=0, sdrplay_api_AGC_CTRL_EN=6 } sdrplay_api_AgcControlT;
typedef struct { sdrplay_api_AgcControlT enable; int setPoint_dBfs; } sdrplay_api_AgcT;
typedef struct { unsigned char enable; unsigned char decimationFactor; unsigned char wideBandSignal; } sdrplay_api_DecimationT;
typedef struct { unsigned char DCenable; unsigned char IQenable; } sdrplay_api_DcOffsetT;
typedef struct { sdrplay_api_DcOffsetT dcOffset; sdrplay_api_DecimationT decimation; sdrplay_api_AgcT agc; } sdrplay_api_ControlParamsT;
typedef struct { unsigned char biasTEnable; } sdrplay_api_Rsp1aTunerParamsT;
typedef struct { sdrplay_api_TunerParamsT tunerParams; sdrplay_api_ControlParamsT ctrlParams; sdrplay_api_Rsp1aTunerParamsT rsp1aTunerParams; } sdrplay_api_RxChannelParamsT;
typedef struct { double fsHz; } sdrplay_api_FsFreqT;
typedef struct { unsigned char rfNotchEnable; unsigned char rfDabNotchEnable; } sdrplay_api_Rsp1aParamsT;
typedef enum { sdrplay_api_ISOCH = 0, sdrplay_api_BULK = 1 } sdrplay_api_TransferModeT;
typedef struct { double ppm; sdrplay_api_FsFreqT fsFreq; sdrplay_api_TransferModeT mode; sdrplay_api_Rsp1aParamsT rsp1aParams; } sdrplay_api_DevParamsT;
typedef struct { sdrplay_api_DevParamsT *devParams; sdrplay_api_RxChannelParamsT *rxChannelA; sdrplay_api_RxChannelParamsT *rxChannelB; } sdrplay_api_DeviceParamsT;
typedef struct { unsigned int firstSampleNum; int grChanged; int rfChanged; int fsChanged; unsigned int numSamples; } sdrplay_api_StreamCbParamsT;
typedef enum { sdrplay_api_GainChange=0, sdrplay_api_PowerOverloadChange=1, sdrplay_api_DeviceRemoved=2, sdrplay_api_RspDuoModeChange=3, sdrplay_api_DeviceFailure=4 } sdrplay_api_EventT;
typedef enum { sdrplay_api_Overload_Detected=0, sdrplay_api_Overload_Corrected=1 } sdrplay_api_PowerOverloadCbEventIdT;
typedef struct { unsigned int gRdB; unsigned int lnaGRdB; double currGain; } sdrplay_api_GainCbParamT;
typedef struct { sdrplay_api_PowerOverloadCbEventIdT powerOverloadChangeType; } sdrplay_api_PowerOverloadCbParamT;
typedef union { sdrplay_api_GainCbParamT gainParams; sdrplay_api_PowerOverloadCbParamT powerOverloadParams; } sdrplay_api_EventParamsT;
typedef void (*sdrplay_api_StreamCallback_t)(short*, short*, sdrplay_api_StreamCbParamsT*, unsigned int, unsigned int, void*);
typedef void (*sdrplay_api_EventCallback_t)(sdrplay_api_EventT, sdrplay_api_TunerSelectT, sdrplay_api_EventParamsT*, void*);
typedef struct { sdrplay_api_StreamCallback_t StreamACbFn; sdrplay_api_StreamCallback_t StreamBCbFn; sdrplay_api_EventCallback_t EventCbFn; } sdrplay_api_CallbackFnsT;
typedef enum { sdrplay_api_Update_None=0, sdrplay_api_Update_Tuner_Gr=0x8000, sdrplay_api_Update_Ctrl_OverloadMsgAck=0x40000000 } sdrplay_api_ReasonForUpdateT;
typedef enum { sdrplay_api_Update_Ext1_None=0 } sdrplay_api_ReasonForUpdateExtension1T;
sdrplay_api_ErrT sdrplay_api_Open(void);
sdrplay_api_ErrT sdrplay_api_Close(void);
sdrplay_api_ErrT sdrplay_api_ApiVersion(float *apiVer);
sdrplay_api_ErrT sdrplay_api_LockDeviceApi(void);
sdrplay_api_ErrT sdrplay_api_UnlockDeviceApi(void);
sdrplay_api_ErrT sdrplay_api_GetDevices(sdrplay_api_DeviceT *devices, unsigned int *numDevs, unsigned int maxDevs);
sdrplay_api_ErrT sdrplay_api_SelectDevice(sdrplay_api_DeviceT *device);
sdrplay_api_ErrT sdrplay_api_ReleaseDevice(sdrplay_api_DeviceT *device);
const char* sdrplay_api_GetErrorString(sdrplay_api_ErrT err);
sdrplay_api_ErrT sdrplay_api_GetDeviceParams(HANDLE dev, sdrplay_api_DeviceParamsT **deviceParams);
sdrplay_api_ErrT sdrplay_api_Init(HANDLE dev, sdrplay_api_CallbackFnsT *callbackFns, void *cbContext);
sdrplay_api_ErrT sdrplay_api_Uninit(HANDLE dev);
sdrplay_api_ErrT sdrplay_api_Update(HANDLE dev, sdrplay_api_TunerSelectT tuner, sdrplay_api_ReasonForUpdateT reasonForUpdate, sdrplay_api_ReasonForUpdateExtension1T reasonForUpdateExt1);
