//! Bindings to libairspy (`airspy.h`, 1.0.x). Only what we call is declared.

use super::{AirspyConfig, AirspyEvents, AirspyGain, DeviceInfo, StreamState};
use crate::{RingSource, SourceError, SourceInfo, ring};
use num_complex::Complex32;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Arc;
use std::time::SystemTime;

#[repr(C)]
struct CDevice {
    _private: [u8; 0],
}

#[repr(C)]
struct Transfer {
    device: *mut CDevice,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: c_int,
    dropped_samples: u64,
    sample_type: c_int,
}

#[repr(C)]
struct PartIdSerial {
    part_id: [u32; 2],
    serial_no: [u32; 4],
}

const SAMPLE_FLOAT32_IQ: c_int = 0;
const AIRSPY_TRUE: c_int = 1;

type SampleBlockFn = unsafe extern "C" fn(*mut Transfer) -> c_int;

#[link(name = "airspy")]
unsafe extern "C" {
    fn airspy_list_devices(serials: *mut u64, count: c_int) -> c_int;
    fn airspy_open_sn(device: *mut *mut CDevice, serial: u64) -> c_int;
    fn airspy_open(device: *mut *mut CDevice) -> c_int;
    fn airspy_close(device: *mut CDevice) -> c_int;
    fn airspy_get_samplerates(device: *mut CDevice, buffer: *mut u32, len: u32) -> c_int;
    fn airspy_set_samplerate(device: *mut CDevice, samplerate: u32) -> c_int;
    fn airspy_set_sample_type(device: *mut CDevice, sample_type: c_int) -> c_int;
    fn airspy_start_rx(device: *mut CDevice, cb: SampleBlockFn, ctx: *mut c_void) -> c_int;
    fn airspy_stop_rx(device: *mut CDevice) -> c_int;
    fn airspy_is_streaming(device: *mut CDevice) -> c_int;
    fn airspy_set_freq(device: *mut CDevice, freq_hz: u32) -> c_int;
    fn airspy_set_lna_gain(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_mixer_gain(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_vga_gain(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_lna_agc(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_mixer_agc(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_linearity_gain(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_sensitivity_gain(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_rf_bias(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_set_packing(device: *mut CDevice, value: u8) -> c_int;
    fn airspy_version_string_read(device: *mut CDevice, version: *mut c_char, len: u8) -> c_int;
    fn airspy_board_partid_serialno_read(device: *mut CDevice, out: *mut PartIdSerial) -> c_int;
    fn airspy_error_name(code: c_int) -> *const c_char;
}

fn error_name(rc: c_int) -> String {
    // SAFETY: libairspy returns a static string for every code.
    let p = unsafe { airspy_error_name(rc) };
    if p.is_null() {
        return format!("error {rc}");
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn check(what: &str, rc: c_int) -> Result<(), SourceError> {
    if rc < 0 {
        Err(SourceError::Device(format!(
            "airspy: {what}: {}",
            error_name(rc)
        )))
    } else {
        Ok(())
    }
}

/// An open (not necessarily streaming) device, closed on drop.
struct Handle(*mut CDevice);

impl Handle {
    fn open(serial: Option<u64>) -> Result<Handle, SourceError> {
        let mut dev = std::ptr::null_mut();
        // SAFETY: dev is a valid out-pointer.
        let rc = unsafe {
            match serial {
                Some(sn) => airspy_open_sn(&mut dev, sn),
                None => airspy_open(&mut dev),
            }
        };
        if rc < 0 || dev.is_null() {
            let which = serial.map_or("an Airspy".into(), |s| format!("Airspy {s:016X}"));
            let hint = if rc == -5 {
                " (not plugged in, or another program has it open)"
            } else {
                ""
            };
            return Err(SourceError::Device(format!(
                "opening {which}: {}{hint}",
                error_name(rc)
            )));
        }
        Ok(Handle(dev))
    }

    fn info(&self) -> Result<DeviceInfo, SourceError> {
        let mut ps = PartIdSerial {
            part_id: [0; 2],
            serial_no: [0; 4],
        };
        // SAFETY: valid device and out-pointer.
        check("reading the serial number", unsafe {
            airspy_board_partid_serialno_read(self.0, &mut ps)
        })?;
        let serial = ((ps.serial_no[2] as u64) << 32) | ps.serial_no[3] as u64;
        let mut ver = [0 as c_char; 128];
        // SAFETY: buffer length as passed; libairspy NUL-terminates.
        let firmware = (unsafe { airspy_version_string_read(self.0, ver.as_mut_ptr(), 127) } >= 0)
            .then(|| {
                unsafe { CStr::from_ptr(ver.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            });
        let mut n = 0u32;
        // SAFETY: with len 0, libairspy writes the count to buffer[0].
        check("listing sample rates", unsafe {
            airspy_get_samplerates(self.0, &mut n, 0)
        })?;
        let mut rates = vec![0u32; n.min(64) as usize];
        if !rates.is_empty() {
            // SAFETY: buffer holds `len` entries.
            check("listing sample rates", unsafe {
                airspy_get_samplerates(self.0, rates.as_mut_ptr(), rates.len() as u32)
            })?;
        }
        Ok(DeviceInfo {
            serial,
            firmware,
            sample_rates: rates,
        })
    }

    fn set_gain(&self, g: AirspyGain) -> Result<(), SourceError> {
        let d = self.0;
        // SAFETY: valid device for all calls below.
        unsafe {
            match g {
                AirspyGain::Linearity(v) => check("setting gain", airspy_set_linearity_gain(d, v)),
                AirspyGain::Sensitivity(v) => {
                    check("setting gain", airspy_set_sensitivity_gain(d, v))
                }
                AirspyGain::Manual { lna, mixer, vga } => {
                    check("setting LNA AGC", airspy_set_lna_agc(d, 0))?;
                    check("setting mixer AGC", airspy_set_mixer_agc(d, 0))?;
                    check("setting LNA gain", airspy_set_lna_gain(d, lna))?;
                    check("setting mixer gain", airspy_set_mixer_gain(d, mixer))?;
                    check("setting VGA gain", airspy_set_vga_gain(d, vga))
                }
                AirspyGain::Agc { vga } => {
                    check("setting LNA AGC", airspy_set_lna_agc(d, 1))?;
                    check("setting mixer AGC", airspy_set_mixer_agc(d, 1))?;
                    check("setting VGA gain", airspy_set_vga_gain(d, vga))
                }
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: opened by airspy_open*, closed once.
        unsafe {
            airspy_close(self.0);
        }
    }
}

/// Devices libairspy can see. Each is opened briefly to read its firmware
/// and sample rates; one that another program has open is listed by serial
/// only.
pub fn list_devices() -> Result<Vec<DeviceInfo>, SourceError> {
    let mut serials = [0u64; 16];
    // SAFETY: buffer sized as passed.
    let n = unsafe { airspy_list_devices(serials.as_mut_ptr(), serials.len() as c_int) };
    check("listing devices", n)?;
    Ok(serials[..(n as usize).min(serials.len())]
        .iter()
        .map(|&sn| {
            Handle::open(Some(sn))
                .and_then(|h| h.info())
                .unwrap_or(DeviceInfo {
                    serial: sn,
                    firmware: None,
                    sample_rates: Vec::new(),
                })
        })
        .collect())
}

struct Context {
    stream: StreamState,
}

unsafe extern "C" fn on_transfer(t: *mut Transfer) -> c_int {
    // SAFETY: libairspy passes a valid transfer for the duration of the
    // call, from its one consumer thread; ctx is the Box<Context> given to
    // start_rx, alive until stop_rx has returned.
    unsafe {
        let t = &*t;
        if t.ctx.is_null() || t.samples.is_null() || t.sample_type != SAMPLE_FLOAT32_IQ {
            return 0;
        }
        let c = &mut *(t.ctx as *mut Context);
        // Complex32 is two f32s (repr(C)), the layout of FLOAT32_IQ.
        let xs = std::slice::from_raw_parts(
            t.samples as *const Complex32,
            t.sample_count.max(0) as usize,
        );
        c.stream.on_samples(xs, t.dropped_samples);
    }
    0
}

/// A streaming Airspy. Samples flow into the ring returned by
/// [`Airspy::start`]; dropping this stops the stream and ends the ring.
pub struct Airspy {
    handle: Handle,
    ctx: *mut Context,
    events: Arc<AirspyEvents>,
    device: DeviceInfo,
}

// SAFETY: libairspy calls are fine from any thread; ctx is only touched by
// the callback while streaming and freed in Drop after stop_rx.
unsafe impl Send for Airspy {}

impl Airspy {
    /// Open a device and start streaming into a new ring of `blocks` blocks
    /// of `block_len` samples.
    pub fn start(
        cfg: &AirspyConfig,
        blocks: usize,
        block_len: usize,
    ) -> Result<(Airspy, RingSource), SourceError> {
        cfg.validate().map_err(SourceError::Device)?;
        let handle = Handle::open(cfg.serial)?;
        let device = handle.info()?;
        let rate = cfg.sample_rate.round() as u32;
        if !device.sample_rates.contains(&rate) {
            let offered: Vec<String> = device
                .sample_rates
                .iter()
                .map(|r| format!("{}", *r as f64 / 1e6))
                .collect();
            return Err(SourceError::Device(format!(
                "this Airspy {} offers {} MS/s, not {} MS/s",
                device.model(),
                offered.join(" or "),
                cfg.sample_rate / 1e6
            )));
        }
        let d = handle.0;
        // SAFETY: valid device. The sample type goes first: libairspy reads
        // it when it sets the rate.
        unsafe {
            check(
                "setting the sample type",
                airspy_set_sample_type(d, SAMPLE_FLOAT32_IQ),
            )?;
            check("setting the sample rate", airspy_set_samplerate(d, rate))?;
            check("setting packing", airspy_set_packing(d, cfg.packing as u8))?;
            check("setting bias-T", airspy_set_rf_bias(d, cfg.bias_t as u8))?;
            check("tuning", airspy_set_freq(d, cfg.tuner_hz()))?;
        }
        handle.set_gain(cfg.gain)?;

        let info = SourceInfo {
            sample_rate: cfg.sample_rate,
            center_freq: cfg.center_hz,
            start_time: None,
            total_samples: None,
            description: String::new(),
        };
        let (producer, mut source) = ring(blocks, block_len, info);
        let events = Arc::new(AirspyEvents::default());
        let ctx = Box::into_raw(Box::new(Context {
            stream: StreamState::new(producer, block_len, events.clone()),
        }));
        let t0 = SystemTime::now();
        // SAFETY: ctx stays alive until Drop, after stop_rx (or is freed
        // below if start fails, when no callback ran).
        let rc = unsafe { airspy_start_rx(d, on_transfer, ctx as *mut c_void) };
        if rc < 0 {
            drop(unsafe { Box::from_raw(ctx) });
            return Err(SourceError::Device(format!(
                "airspy: starting the stream: {}",
                error_name(rc)
            )));
        }
        source.set_info(SourceInfo {
            sample_rate: cfg.sample_rate,
            center_freq: cfg.center_hz,
            start_time: Some(t0),
            total_samples: None,
            description: format!(
                "Airspy {} {:016X} (gain {}{})",
                device.model(),
                device.serial,
                cfg.gain,
                if cfg.bias_t { ", bias-T" } else { "" }
            ),
        });
        Ok((
            Airspy {
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

    pub fn events(&self) -> &Arc<AirspyEvents> {
        &self.events
    }

    /// Whether libairspy is still streaming (false once the device is
    /// unplugged or its USB transfers fail).
    pub fn is_streaming(&self) -> bool {
        // SAFETY: valid device until Drop.
        unsafe { airspy_is_streaming(self.handle.0) == AIRSPY_TRUE }
    }

    /// Change gain while streaming.
    pub fn set_gain(&mut self, g: AirspyGain) -> Result<(), SourceError> {
        g.validate().map_err(SourceError::Device)?;
        self.handle.set_gain(g)
    }
}

impl Drop for Airspy {
    fn drop(&mut self) {
        // SAFETY: after stop_rx returns libairspy's threads have exited, so
        // no callback runs and ctx can be reclaimed. The handle closes after.
        unsafe {
            airspy_stop_rx(self.handle.0);
            let mut ctx = Box::from_raw(self.ctx);
            ctx.stream.close();
        }
    }
}
