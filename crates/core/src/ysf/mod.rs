//! Yaesu System Fusion (C4FM): 4FSK at 4800 symbols/s in 12.5 kHz, one
//! 100 ms frame after another, each opening with a sync and a FICH. See
//! PLAN.md phase 6.
//!
//! - [`rx`]: discriminator output to frames (symbol recovery, sync, FICH).
//! - [`calls`]: frames to calls, with callsigns, DG-ID and AMBE+2 voice.
//! - [`frame`], [`fec`]: the frame layout and its channel coding.
//! - [`tx`]: a transmitter for tests.
//!
//! Voice in DN (V/D mode 2) and V/D mode 1 is AMBE+2, the same codec as
//! DMR, so it goes through [`crate::dmr::vocoder`] and its raw frames are
//! kept the same way. VW (voice full rate) is IMBE: those calls are
//! recorded with their callsigns but no voice yet.

pub mod calls;
pub mod fec;
pub mod frame;
pub mod rx;
pub mod tx;

pub use calls::{CallEvent, CallInfo};

#[cfg(test)]
mod tests {
    use super::calls::{CallEvent, Calls};
    use super::frame::DataType;
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

    fn padded(syms: crate::dmr::tx::Symbols) -> crate::dmr::tx::Symbols {
        let mut v = vec![None; 3000];
        v.extend(syms);
        v.extend(vec![None; 3000]);
        v
    }

    fn decode(iq: &[Complex32], rate: f64) -> (Vec<CallEvent>, Receiver) {
        let mut rx = Receiver::new(rate);
        let mut calls = Calls::new();
        let mut frames = Vec::new();
        let mut ev = Vec::new();
        for c in iq.chunks(4000) {
            rx.push(c, &mut frames);
            for f in frames.drain(..) {
                calls.push(&f, &mut ev);
            }
        }
        calls.end_all(&mut ev);
        (ev, rx)
    }

    fn voice(ev: &[CallEvent]) -> Vec<[u8; 49]> {
        ev.iter()
            .filter_map(|e| match e {
                CallEvent::Voice { frames } => {
                    Some(frames.iter().map(|f| f.bits).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn ends(ev: &[CallEvent]) -> Vec<super::CallInfo> {
        ev.iter()
            .filter_map(|e| match e {
                CallEvent::End { info, .. } => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    /// A header from an independent implementation (MMDVMHost's encoders):
    /// DG-ID 21, CSD1 `ALL`/`KF7HVM`, CSD2 `W7ABC`/`N0CALL`.
    const REF_HEADER: &str = "d471c9634d003ca5e9b10023937e9d0001d666b600314a513f0011ffa0ecf7235cfc15a07f31b8d0e35f3c10bf3f33f4b5185ab4f6971c14b6bf255abfb29bc494bacdaf0b7fbcf91b8ec6a9ad5a7f8251143f062f3303702511c999b8314f0541653e499d4d13443607bbd586b19cd37c36324bd5923187";

    #[test]
    fn matches_reference_header() {
        use super::frame::{self, Fi, Fich, callsign_field};
        let bytes: Vec<u8> = (0..REF_HEADER.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&REF_HEADER[i..i + 2], 16).unwrap())
            .collect();
        let b = crate::dmr::fec::bytes_to_bits(&bytes);
        let want = Fich {
            fi: Fi::Header,
            cm: 0,
            fn_: 0,
            ft: 6,
            dt: DataType::VdMode1,
            mr: 0,
            dg_id: 21,
        };
        assert_eq!(frame::fich(&b), Some(want));
        let csd1 = [callsign_field("ALL"), callsign_field("KF7HVM")].concat();
        let csd2 = [callsign_field("W7ABC"), callsign_field("N0CALL")].concat();
        assert_eq!(frame::dch(&b, true, false), Some(csd1.clone()));
        assert_eq!(frame::dch(&b, true, true), Some(csd2.clone()));
        // Our encoder makes the same bits.
        let mut ours = [0u8; frame::FRAME_BITS];
        frame::set_sync(&mut ours);
        frame::set_fich(&mut ours, want);
        frame::set_dch(&mut ours, &csd1, true, false);
        frame::set_dch(&mut ours, &csd2, true, true);
        assert_eq!(&ours[..], &b[..]);
    }

    #[test]
    fn dn_call_decodes_callsigns_and_voice() {
        let mut call = TxCall::dn("KF7HVM", 12);
        call.dg_id = 21;
        call.downlink = "W7ABC".into();
        call.uplink = "W7ABC".into();
        let rate = 31_250.0;
        let mut iq = tx::modulate(&padded(tx::symbols(&tx::call_frames(&call))), rate, 0.5);
        shift(&mut iq, -600.0, rate);
        add_noise(&mut iq, 0.03, 5);
        let (ev, _) = decode(&iq, rate);
        let starts = ev
            .iter()
            .filter(|e| matches!(e, CallEvent::Start { .. }))
            .count();
        assert_eq!(starts, 1, "{ev:?}");
        let CallEvent::Start { info, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        // The header came first, so the callsigns are known from the start.
        assert_eq!(info.src.as_deref(), Some("KF7HVM"));
        let e = ends(&ev);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].src.as_deref(), Some("KF7HVM"));
        assert_eq!(e[0].dst.as_deref(), Some("ALL"));
        assert_eq!(e[0].downlink.as_deref(), Some("W7ABC"));
        assert_eq!(e[0].dg_id, Some(21));
        assert_eq!(e[0].mode(), Some("dn"));
        let v = voice(&ev);
        assert_eq!(v.len(), 12 * 5);
        for (n, f) in v.iter().enumerate() {
            assert_eq!(*f, test_frame(n), "frame {n}");
        }
    }

    #[test]
    fn late_entry_learns_source_from_voice_frames() {
        let mut call = TxCall::dn("N0CALL", 9);
        call.header = false;
        call.terminator = false;
        let rate = 48_000.0;
        let iq = tx::modulate(&padded(tx::symbols(&tx::call_frames(&call))), rate, 0.5);
        let (ev, _) = decode(&iq, rate);
        let CallEvent::Start { info, .. } = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(info.src, None);
        let e = ends(&ev);
        assert_eq!(e.len(), 1, "{ev:?}");
        assert_eq!(e[0].src.as_deref(), Some("N0CALL"));
        assert_eq!(voice(&ev).len(), 9 * 5);
    }

    #[test]
    fn vd_mode1_narrow_deviation_and_inverted() {
        let mut call = TxCall::dn("KF7HVM", 7);
        call.dt = DataType::VdMode1;
        let rate = 25_000.0;
        let mut iq = crate::dmr::tx::modulate_4fsk(
            &padded(tx::symbols(&tx::call_frames(&call))),
            rate,
            0.5,
            1800.0,
        );
        // A receiver with the spectrum flipped.
        for v in &mut iq {
            *v = v.conj();
        }
        add_noise(&mut iq, 0.02, 9);
        let (ev, _) = decode(&iq, rate);
        let e = ends(&ev);
        assert_eq!(e.len(), 1, "{ev:?}");
        assert_eq!(e[0].src.as_deref(), Some("KF7HVM"));
        assert_eq!(e[0].mode(), Some("vd1"));
        let v = voice(&ev);
        assert_eq!(v.len(), 7 * 5);
        for (n, f) in v.iter().enumerate() {
            assert_eq!(*f, test_frame(n), "frame {n}");
        }
    }

    #[test]
    fn noise_and_fm_find_no_frames() {
        let rate = 31_250.0;
        let mut iq: Vec<Complex32> = (0..(rate as usize * 3))
            .map(|i| {
                let t = i as f64 / rate;
                let ph = 3000.0 / 1000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
                Complex32::from_polar(0.3, ph as f32)
            })
            .collect();
        add_noise(&mut iq, 0.3, 3);
        let (ev, rx) = decode(&iq, rate);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(rx.syncs, 0);
    }
}
