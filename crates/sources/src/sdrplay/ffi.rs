//! Bindings to `shim.c` (see `crates/sources/sdrplay/scanner_sdrplay.h`).

use super::{DeviceEvents, DeviceInfo, SdrplayConfig, StreamState};
use crate::{RingSource, SourceError, SourceInfo, ring};
use std::ffi::{CStr, c_char, c_double, c_int, c_short, c_uint, c_void};
use std::sync::Arc;
use std::time::SystemTime;

#[repr(C)]
struct CDevice {
    serial: [c_char; 64],
    model: [c_char; 32],
    hw_ver: c_int,
}

#[repr(C)]
struct CConfig {
    serial: [c_char; 64],
    sample_rate: c_double,
    center_hz: c_double,
    bandwidth_khz: c_int,
    lna_state: c_int,
    gr_db: c_int,
    agc: c_int,
    agc_setpoint_dbfs: c_int,
    ppm: c_double,
    bias_t: c_int,
    rf_notch: c_int,
    dab_notch: c_int,
}

#[repr(C)]
struct CHandle {
    _private: [u8; 0],
}

type SamplesFn =
    unsafe extern "C" fn(*mut c_void, *const c_short, *const c_short, c_uint, c_uint, c_int);
type EventFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_double);

unsafe extern "C" {
    fn scn_sdrplay_list(out: *mut CDevice, max: c_int, err: *mut c_char, errlen: c_int) -> c_int;
    fn scn_sdrplay_start(
        cfg: *const CConfig,
        on_samples: SamplesFn,
        on_event: EventFn,
        ctx: *mut c_void,
        out: *mut *mut CHandle,
        chosen: *mut CDevice,
        err: *mut c_char,
        errlen: c_int,
    ) -> c_int;
    fn scn_sdrplay_set_gain(
        h: *mut CHandle,
        lna_state: c_int,
        gr_db: c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> c_int;
    fn scn_sdrplay_stop(h: *mut CHandle);
}

const ERRLEN: usize = 512;

fn c_str(buf: &[c_char]) -> String {
    // SAFETY: the shim always NUL-terminates (snprintf), and the buffers are
    // zeroed before use.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn device_error(err: &[c_char]) -> SourceError {
    SourceError::Device(c_str(err))
}

impl CDevice {
    fn zeroed() -> Self {
        Self {
            serial: [0; 64],
            model: [0; 32],
            hw_ver: 0,
        }
    }
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            serial: c_str(&self.serial),
            model: c_str(&self.model),
            hw_ver: self.hw_ver,
        }
    }
}

/// Devices the API can see (opening the API briefly).
pub fn list_devices() -> Result<Vec<DeviceInfo>, SourceError> {
    let mut devs: Vec<CDevice> = (0..16).map(|_| CDevice::zeroed()).collect();
    let mut err = [0 as c_char; ERRLEN];
    // SAFETY: buffers are sized as passed.
    let n = unsafe { scn_sdrplay_list(devs.as_mut_ptr(), 16, err.as_mut_ptr(), ERRLEN as c_int) };
    if n < 0 {
        return Err(device_error(&err));
    }
    Ok(devs[..(n as usize).min(16)]
        .iter()
        .map(CDevice::info)
        .collect())
}

struct Context {
    stream: StreamState,
    events: Arc<DeviceEvents>,
}

unsafe extern "C" fn on_samples(
    ctx: *mut c_void,
    xi: *const c_short,
    xq: *const c_short,
    n: c_uint,
    first: c_uint,
    reset: c_int,
) {
    if ctx.is_null() || xi.is_null() || xq.is_null() {
        return;
    }
    // SAFETY: ctx is the Box<Context> passed to start, alive until stop
    // returns; the API calls the stream callback from one thread at a time.
    // xi and xq point to n samples each for the duration of the call.
    let (c, xi, xq) = unsafe {
        (
            &mut *(ctx as *mut Context),
            std::slice::from_raw_parts(xi, n as usize),
            std::slice::from_raw_parts(xq, n as usize),
        )
    };
    c.stream.on_samples(xi, xq, first, reset != 0);
}

unsafe extern "C" fn on_event(ctx: *mut c_void, kind: c_int, a: c_int, b: c_int, _gain: c_double) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: see on_samples. Only the shared, atomic `events` is touched
    // here, because events arrive on a different thread from samples.
    let events = unsafe { &(*(ctx as *const Context)).events };
    events.on_event(kind, a, b);
    match kind {
        2 => eprintln!("sdrplay: overload detected (reduce gain: raise --lna or --gr)"),
        4 => eprintln!("sdrplay: device removed"),
        5 => eprintln!("sdrplay: device failure"),
        _ => {}
    }
}

/// A streaming SDRplay device. Samples flow into the ring returned by
/// [`Sdrplay::start`]; dropping this stops the stream and ends the ring.
pub struct Sdrplay {
    handle: *mut CHandle,
    ctx: *mut Context,
    events: Arc<DeviceEvents>,
    device: DeviceInfo,
}

// SAFETY: the handle is only used through the shim, which the API allows from
// any thread; ctx is only freed in Drop after the API has stopped calling back.
unsafe impl Send for Sdrplay {}

impl Sdrplay {
    /// Open a device and start streaming into a new ring of `blocks` blocks
    /// of `block_len` samples.
    pub fn start(
        cfg: &SdrplayConfig,
        blocks: usize,
        block_len: usize,
    ) -> Result<(Sdrplay, RingSource), SourceError> {
        cfg.validate().map_err(SourceError::Device)?;
        let mut c = CConfig {
            serial: [0; 64],
            sample_rate: cfg.sample_rate,
            center_hz: cfg.center_hz,
            bandwidth_khz: cfg.bandwidth_khz as c_int,
            lna_state: cfg.lna_state as c_int,
            gr_db: cfg.gr_db as c_int,
            agc: cfg.agc_setpoint_dbfs.is_some() as c_int,
            agc_setpoint_dbfs: cfg.agc_setpoint_dbfs.unwrap_or(-30),
            ppm: cfg.ppm,
            bias_t: cfg.bias_t as c_int,
            rf_notch: cfg.rf_notch as c_int,
            dab_notch: cfg.dab_notch as c_int,
        };
        if let Some(s) = &cfg.serial {
            for (d, b) in c.serial.iter_mut().zip(s.bytes()) {
                *d = b as c_char;
            }
        }
        let info = SourceInfo {
            sample_rate: cfg.sample_rate,
            center_freq: cfg.center_hz,
            start_time: None,
            total_samples: None,
            description: String::new(),
        };
        let (producer, source) = ring(blocks, block_len, info);
        let events = Arc::new(DeviceEvents::default());
        let ctx = Box::into_raw(Box::new(Context {
            stream: StreamState::new(producer, block_len, cfg.sample_rate, events.clone()),
            events: events.clone(),
        }));
        let mut handle = std::ptr::null_mut();
        let mut chosen = CDevice::zeroed();
        let mut err = [0 as c_char; ERRLEN];
        // Stamp the start just before streaming begins.
        let t0 = SystemTime::now();
        // SAFETY: all pointers are valid for the call; ctx stays alive until
        // Drop (or is freed below if start fails, when no callback ran).
        let rc = unsafe {
            scn_sdrplay_start(
                &c,
                on_samples,
                on_event,
                ctx as *mut c_void,
                &mut handle,
                &mut chosen,
                err.as_mut_ptr(),
                ERRLEN as c_int,
            )
        };
        if rc != 0 {
            // SAFETY: start failed, so the API holds no reference to ctx.
            drop(unsafe { Box::from_raw(ctx) });
            return Err(device_error(&err));
        }
        let device = chosen.info();
        let mut source = source;
        source.set_info(SourceInfo {
            sample_rate: cfg.sample_rate,
            center_freq: cfg.center_hz,
            start_time: Some(t0),
            total_samples: None,
            description: format!(
                "SDRplay {} {} (LNA {}, GR {} dB{})",
                device.model,
                device.serial,
                cfg.lna_state,
                cfg.gr_db,
                if cfg.agc_setpoint_dbfs.is_some() {
                    ", AGC"
                } else {
                    ""
                }
            ),
        });
        Ok((
            Sdrplay {
                handle,
                ctx,
                events,
                device,
            },
            source,
        ))
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn events(&self) -> &Arc<DeviceEvents> {
        &self.events
    }

    /// Change gain while streaming.
    pub fn set_gain(&mut self, lna_state: u8, gr_db: u8) -> Result<(), SourceError> {
        if !(20..=59).contains(&gr_db) {
            return Err(SourceError::Device(format!(
                "IF gain reduction {gr_db} dB is outside 20 to 59"
            )));
        }
        let mut err = [0 as c_char; ERRLEN];
        // SAFETY: handle is live until Drop.
        let rc = unsafe {
            scn_sdrplay_set_gain(
                self.handle,
                lna_state as c_int,
                gr_db as c_int,
                err.as_mut_ptr(),
                ERRLEN as c_int,
            )
        };
        if rc != 0 {
            return Err(device_error(&err));
        }
        Ok(())
    }
}

impl Drop for Sdrplay {
    fn drop(&mut self) {
        // SAFETY: after scn_sdrplay_stop returns no callback runs, so ctx can
        // be reclaimed.
        unsafe {
            scn_sdrplay_stop(self.handle);
            let mut ctx = Box::from_raw(self.ctx);
            ctx.stream.close();
        }
    }
}
