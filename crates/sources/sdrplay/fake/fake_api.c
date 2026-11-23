#include <unistd.h>
/* Fake SDRplay API (built against the stand-in header next to it) for an end-to-end test: streams a 147.52 MHz carrier
 * keyed on/off, 1008-sample callbacks, with one injected counter gap.
 * FAKE_STALL_AFTER=<s>: the first stream stops delivering samples after that many seconds (like a USB failure), and the
 * first SelectDevice after that fails once, so a reopen has to retry. */
#include <stdlib.h>
#include <sdrplay_api.h>
#include <math.h>
#include <pthread.h>
#include <string.h>
#include <stdio.h>
#include <time.h>
static sdrplay_api_DevParamsT devp; static sdrplay_api_RxChannelParamsT rxa;
static sdrplay_api_DeviceParamsT params = {&devp, &rxa, 0};
static sdrplay_api_CallbackFnsT cbs; static void *cbctx; static volatile int running; static pthread_t th;
static int inits, selects;
static double stall_after(void){ const char *s=getenv("FAKE_STALL_AFTER"); return s ? atof(s) : 0; }
sdrplay_api_ErrT sdrplay_api_Open(void){return 0;}
sdrplay_api_ErrT sdrplay_api_Close(void){return 0;}
sdrplay_api_ErrT sdrplay_api_ApiVersion(float *v){*v=3.15f;return 0;}
/* FAKE_HANG_LOCK=1: another program holds the API lock, so this never returns. */
sdrplay_api_ErrT sdrplay_api_LockDeviceApi(void){ if(getenv("FAKE_HANG_LOCK")) for(;;) sleep(1000); return 0;}
sdrplay_api_ErrT sdrplay_api_UnlockDeviceApi(void){return 0;}
sdrplay_api_ErrT sdrplay_api_GetDevices(sdrplay_api_DeviceT *d, unsigned int *n, unsigned int m){memset(d,0,sizeof *d);strcpy(d->SerNo,"FAKE0001");d->hwVer=SDRPLAY_RSP1A_ID;d->dev=(HANDLE)1;*n=1;return 0;}
sdrplay_api_ErrT sdrplay_api_SelectDevice(sdrplay_api_DeviceT *d){
  if(++selects==2 && stall_after()>0){ fprintf(stderr,"fake: refusing the first reopen\n"); return 1; }
  return 0;}
sdrplay_api_ErrT sdrplay_api_ReleaseDevice(sdrplay_api_DeviceT *d){return 0;}
const char* sdrplay_api_GetErrorString(sdrplay_api_ErrT e){return "fake error";}
sdrplay_api_ErrT sdrplay_api_GetDeviceParams(HANDLE dev, sdrplay_api_DeviceParamsT **p){*p=&params;return 0;}
static void *run(void *x){
  double fs=devp.fsFreq.fsHz, off=147.52e6-rxa.tunerParams.rfFreq.rfHz, ph=0; unsigned int first=0; short xi[1008],xq[1008];
  struct timespec t0; clock_gettime(CLOCK_MONOTONIC,&t0); unsigned long long sent=0; int ncb=0;
  fprintf(stderr,"fake: mode=%d fs=%.0f rf=%.0f lna=%d gr=%d bw=%d\n",devp.mode,fs,rxa.tunerParams.rfFreq.rfHz,rxa.tunerParams.gain.LNAstate,rxa.tunerParams.gain.gRdB,rxa.tunerParams.bwType);
  unsigned int seed=1; int session=inits; double stall=stall_after();
  while(running){
    if(session==1 && stall>0 && sent/fs>stall){ struct timespec s={0,10000000}; nanosleep(&s,0); continue; }
    double t=sent/fs; int on = fmod(t,4.0)>1.0 && fmod(t,4.0)<3.0;
    for(int k=0;k<1008;k++){ seed=seed*1103515245+12345; double nI=((int)(seed>>16&0x7fff)-16384)/16384.0*200; seed=seed*1103515245+12345; double nQ=((int)(seed>>16&0x7fff)-16384)/16384.0*200;
      ph+=2*M_PI*(off+(on?3000*sin(2*M_PI*1000*(sent+k)/fs):0))/fs; if(ph>M_PI)ph-=2*M_PI;
      xi[k]=(short)(nI+(on?4000*cos(ph):0)); xq[k]=(short)(nQ+(on?4000*sin(ph):0)); }
    if(++ncb==5000){ first+=1008*3; sent+=1008*3; } /* lose 3 packets */
    sdrplay_api_StreamCbParamsT p={first,0,0,0,1008};
    cbs.StreamACbFn(xi,xq,&p,1008,ncb==1,cbctx); first+=1008; sent+=1008;
    if(ncb==3000){ sdrplay_api_EventParamsT e; e.powerOverloadParams.powerOverloadChangeType=sdrplay_api_Overload_Detected; cbs.EventCbFn(sdrplay_api_PowerOverloadChange,sdrplay_api_Tuner_A,&e,cbctx);}
    struct timespec now; clock_gettime(CLOCK_MONOTONIC,&now); double el=(now.tv_sec-t0.tv_sec)+(now.tv_nsec-t0.tv_nsec)*1e-9; double due=sent/fs;
    if(due>el){ struct timespec s={0,(long)((due-el)*1e9)}; nanosleep(&s,0);} }
  return 0; }
sdrplay_api_ErrT sdrplay_api_Init(HANDLE dev, sdrplay_api_CallbackFnsT *c, void *ctx){cbs=*c;cbctx=ctx;running=1;inits++;pthread_create(&th,0,run,0);return 0;}
sdrplay_api_ErrT sdrplay_api_Uninit(HANDLE dev){running=0;pthread_join(th,0);return 0;}
sdrplay_api_ErrT sdrplay_api_Update(HANDLE dev, sdrplay_api_TunerSelectT t, sdrplay_api_ReasonForUpdateT r, sdrplay_api_ReasonForUpdateExtension1T e){fprintf(stderr,"fake: update 0x%x\n",r);return 0;}
