//! D-STAR: GMSK at 4800 bit/s. A transmission is a header (callsigns,
//! strongly protected), then 20 ms frames of AMBE voice and slow data, then
//! an end pattern. See PLAN.md phase 6.
//!
//! - [`rx`]: discriminator output to headers and frames.
//! - [`calls`]: those to calls, with callsigns, message and voice.
//! - [`header`], [`frame`]: the header's coding, the frame layout, slow data.
//! - [`tx`]: a transmitter for tests.
//!
//! Voice is DVSI's AMBE (3600x2400), older than DMR's AMBE+2 but with the
//! same frame coding, so frames are kept the same way (`.amb`, marked as
//! D-STAR) and decoded by [`crate::dmr::vocoder`] with
//! [`crate::dmr::vocoder::Codec::Ambe`].

pub mod calls;
pub mod frame;
pub mod header;
pub mod rx;
pub mod tx;

pub use calls::{CallEvent, CallInfo};
pub use header::Header;

#[cfg(test)]
mod tests {
    use super::calls::{CallEvent, Calls};
    use super::rx::Receiver;
    use super::tx::{self, TxCall};
    use crate::dmr::tx::test_frame;
    use num_complex::Complex32;

    fn add_noise(x: &mut [Complex32], sigma: f32, seed: u64) {
        let mut s = seed | 1;
        let mut uni = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
        };
        for v in x {
            let (u1, u2) = (uni(), uni());
            let r = (-2.0 * u1.ln()).sqrt() as f32 * sigma;
            let t = (2.0 * std::f64::consts::PI * u2) as f32;
            *v += Complex32::new(r * t.cos(), r * t.sin());
        }
    }

    fn shift(x: &mut [Complex32], hz: f64, rate: f64) {
        for (i, v) in x.iter_mut().enumerate() {
            *v *= Complex32::from_polar(
                1.0,
                (2.0 * std::f64::consts::PI * hz * i as f64 / rate) as f32,
            );
        }
    }

    fn decode(iq: &[Complex32], rate: f64) -> (Vec<CallEvent>, Receiver) {
        let mut rx = Receiver::new(rate);
        let mut calls = Calls::new();
        let mut evs = Vec::new();
        let mut out = Vec::new();
        for c in iq.chunks(4000) {
            rx.push(c, &mut evs);
            for e in evs.drain(..) {
                calls.push(&e, &mut out);
            }
        }
        calls.end_all(&mut out);
        (out, rx)
    }

    fn ends(ev: &[CallEvent]) -> Vec<super::CallInfo> {
        ev.iter()
            .filter_map(|e| match e {
                CallEvent::End { info, .. } => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    fn voice(ev: &[CallEvent]) -> Vec<[u8; 49]> {
        ev.iter()
            .filter_map(|e| match e {
                CallEvent::Voice { frame } => Some(frame.bits),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn call_decodes_header_message_and_voice() {
        let call = TxCall::new("KF7HVM", 70);
        let rate = 31_250.0;
        let mut iq = tx::modulate(&tx::call_bits(&call), rate, 0.5, 2000);
        shift(&mut iq, 500.0, rate);
        add_noise(&mut iq, 0.03, 4);
        let (ev, _) = decode(&iq, rate);
        let starts = ev
            .iter()
            .filter(|e| matches!(e, CallEvent::Start { .. }))
            .count();
        assert_eq!(starts, 1, "{:?}", ends(&ev));
        let e = ends(&ev);
        assert_eq!(e.len(), 1);
        let h = e[0].header.as_ref().expect("header");
        assert_eq!(h.my.as_deref(), Some("KF7HVM"));
        assert_eq!(h.suffix.as_deref(), Some("ID51"));
        assert_eq!(h.your.as_deref(), Some("CQCQCQ"));
        assert_eq!(h.rpt1.as_deref(), Some("WB7DZG B"));
        assert_eq!(e[0].message.as_deref(), Some("Hello from Rainier"));
        let v = voice(&ev);
        assert_eq!(v.len(), 70);
        for (n, f) in v.iter().enumerate() {
            assert_eq!(*f, test_frame(n), "frame {n}");
        }
    }

    #[test]
    fn late_entry_gets_the_header_from_slow_data() {
        let mut call = TxCall::new("N0CALL", 130);
        call.send_header = false;
        let rate = 48_000.0;
        let mut iq = tx::modulate(&tx::call_bits(&call), rate, 0.5, 2000);
        // Spectrum flipped.
        for v in &mut iq {
            *v = v.conj();
        }
        add_noise(&mut iq, 0.02, 8);
        let (ev, _) = decode(&iq, rate);
        let e = ends(&ev);
        assert_eq!(e.len(), 1, "{e:?}");
        let h = e[0].header.as_ref().expect("header from slow data");
        assert_eq!(h.my.as_deref(), Some("N0CALL"));
        // Frame 0 carried the first sync; the rest are all there.
        let v = voice(&ev);
        assert_eq!(v.len(), 129);
        assert_eq!(v[0], test_frame(1));
    }

    #[test]
    fn noise_and_fm_find_nothing() {
        let rate = 31_250.0;
        let mut iq: Vec<Complex32> = (0..(rate as usize * 3))
            .map(|i| {
                let t = i as f64 / rate;
                let ph = 3000.0 / 1000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
                Complex32::from_polar(0.3, ph as f32)
            })
            .collect();
        add_noise(&mut iq, 0.3, 3);
        let (ev, _) = decode(&iq, rate);
        assert!(ev.is_empty(), "{ev:?}");
    }
}
