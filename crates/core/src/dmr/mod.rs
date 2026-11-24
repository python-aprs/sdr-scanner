//! DMR (ETSI TS 102 361) voice: 4FSK at 4800 symbols/s, two TDMA timeslots
//! in 12.5 kHz. See PLAN.md phase 6.
//!
//! - [`rx`]: discriminator output to bursts (symbol recovery, sync, framing).
//! - [`calls`]: bursts to calls per timeslot, with link control, talker alias
//!   and AMBE+2 voice frames.
//! - [`vocoder`]: voice frames to audio. AMBE+2 is DVSI's codec; audio needs
//!   the optional `mbelib` build. Without it, recordings keep the raw frames
//!   (`.amb`, as dsd writes them) so they can be decoded later.
//! - [`tx`]: a transmitter for tests.

pub mod burst;
pub mod calls;
pub mod fec;
pub mod lc;
pub mod rx;
pub mod tx;
pub mod vocoder;

pub use burst::AmbeFrame;
pub use calls::{CallEvent, CallInfo};
pub use lc::Lc;

#[cfg(test)]
mod tests {
    use super::burst::Source;
    use super::calls::{CallEvent, Calls};
    use super::rx::Receiver;
    use super::tx::{self, TxCall};
    use num_complex::Complex32;

    /// Deterministic complex noise of standard deviation `sigma` per part.
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
        let mut bursts = Vec::new();
        let mut ev = Vec::new();
        for c in iq.chunks(4000) {
            rx.push(c, &mut bursts);
            for b in bursts.drain(..) {
                calls.push(&b, &mut ev);
            }
        }
        calls.end_all(&mut ev);
        (ev, rx)
    }

    fn padded(mut syms: tx::Symbols) -> tx::Symbols {
        let mut v: tx::Symbols = vec![None; 2000];
        v.append(&mut syms);
        v.extend(vec![None; 2000]);
        v
    }

    fn voice_frames(ev: &[CallEvent], key: u8) -> Vec<super::AmbeFrame> {
        ev.iter()
            .filter_map(|e| match e {
                CallEvent::Voice { key: k, frames } if *k == key => Some(frames.to_vec()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn hex_bits(h: &str) -> Vec<u8> {
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect();
        super::fec::bytes_to_bits(&bytes)
    }

    /// Bursts from an independent implementation (MMDVMHost's encoders):
    /// group call TG 99 from 3171501, colour code 1, mobile data sync.
    const REF_HEADER: &str = "00a8035c01bc20687d505ca0c46d5d7f77fd757e316416782be02b8069413a028c";
    const REF_TERMINATOR: &str =
        "00c70388010c20107d205c00c4ad5d7f77fd7579647015402c8027c061412302bf";
    const REF_VOICE: [&str; 4] = [
        "000000000000000000000000000130900030091000000000000000000000000000",
        "000000000000000000000000000170a0c060374000000000000000000000000000",
        "00000000000000000000000000017091e220c74000000000000000000000000000",
        "00000000000000000000000000015183a273507000000000000000000000000000",
    ];

    #[test]
    fn matches_reference_bursts() {
        use super::burst::{self, DataType, Emb};
        use super::{fec, lc};
        let want = super::Lc::group(99, 3_171_501);
        for (h, dt, mask) in [
            (REF_HEADER, DataType::VoiceLcHeader, lc::MASK_VOICE_HEADER),
            (
                REF_TERMINATOR,
                DataType::TerminatorWithLc,
                lc::MASK_TERMINATOR,
            ),
        ] {
            let b = hex_bits(h);
            assert_eq!(burst::slot_type(&b), Some((1, dt)));
            let (bits, clean) = fec::bptc_decode(&burst::info_bits(&b));
            assert!(clean);
            assert_eq!(lc::full_lc(&bits, mask), Some(want));
            // Our encoder makes the same bits.
            let mut ours = [0u8; 264];
            burst::set_info_bits(&mut ours, &fec::bptc_encode(&lc::full_lc_bits(&want, mask)));
            burst::set_slot_type(&mut ours, 1, dt);
            burst::set_sync(&mut ours, 0xD5D7_F77F_D757);
            assert_eq!(&ours[..], &b[..]);
        }
        let mut raw = [0u8; 128];
        for (i, h) in REF_VOICE.iter().enumerate() {
            let b = hex_bits(h);
            let lcss = [1, 3, 3, 2][i];
            assert_eq!(
                burst::emb(&b),
                Some(Emb {
                    cc: 1,
                    pi: false,
                    lcss
                })
            );
            raw[i * 32..i * 32 + 32].copy_from_slice(&burst::embedded_bits(&b));
        }
        assert_eq!(fec::embedded_decode(&raw), Some(want.to_bytes()));
        assert_eq!(fec::embedded_encode(&want.to_bytes()), raw);
    }

    #[test]
    fn simplex_call_decodes_ids_alias_and_voice() {
        let mut call = TxCall::group(99, 3_171_501, 6);
        call.alias = Some("KF7HVM".into());
        let bursts = tx::call_bursts(&call, Source::Mobile);
        let rate = 31_250.0;
        let mut iq = tx::modulate(&padded(tx::mobile_symbols(&bursts)), rate, 0.5);
        shift(&mut iq, 400.0, rate);
        add_noise(&mut iq, 0.02, 7);
        let (ev, _) = decode(&iq, rate);
        let starts: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, CallEvent::Start { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "{ev:?}");
        let CallEvent::Start { key, info, .. } = starts[0] else {
            unreachable!()
        };
        // The header came first, so the ids are known from the start.
        assert_eq!(info.lc.map(|l| (l.dst, l.src)), Some((99, 3_171_501)));
        assert_eq!(info.timeslot, None);
        let end = ev.iter().find_map(|e| match e {
            CallEvent::End { info, .. } => Some(info.clone()),
            _ => None,
        });
        let end = end.expect("call ended");
        assert_eq!(end.call_type(), Some("group"));
        assert_eq!(end.color_code, Some(1));
        assert!(!end.encrypted && !end.repeater);
        assert_eq!(end.alias.as_deref(), Some("KF7HVM"));
        let frames = voice_frames(&ev, *key);
        assert_eq!(frames.len(), 6 * 6 * 3);
        for (n, f) in frames.iter().enumerate() {
            assert_eq!(f.bits, tx::test_frame(n), "frame {n}");
        }
    }

    #[test]
    fn late_entry_learns_ids_from_embedded_lc() {
        let mut call = TxCall::group(3153, 1_234_567, 4);
        call.headers = 0;
        call.terminator = false;
        let bursts = tx::call_bursts(&call, Source::Mobile);
        let rate = 48_000.0;
        let iq = tx::modulate(&padded(tx::mobile_symbols(&bursts)), rate, 0.5);
        let (ev, _) = decode(&iq, rate);
        let infos: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                CallEvent::Start { info, .. } => Some((true, info.clone())),
                CallEvent::End { info, .. } => Some((false, info.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(infos.len(), 2, "{ev:?}");
        assert_eq!(infos[0].1.lc, None);
        assert_eq!(
            infos[1].1.lc.map(|l| (l.dst, l.src)),
            Some((3153, 1_234_567))
        );
    }

    #[test]
    fn repeater_carries_two_calls_at_once() {
        let a = tx::call_bursts(&TxCall::group(91, 1_111_111, 3), Source::Base);
        let mut c2 = TxCall::group(3153, 2_222_222, 2);
        c2.lc.flco = super::lc::FLCO_PRIVATE;
        let b = tx::call_bursts(&c2, Source::Base);
        // TS2's call starts a few frames late.
        let mut b2 = vec![tx::idle_burst(1); 4];
        b2.extend(b);
        let rate = 25_000.0;
        let mut iq = tx::modulate(&padded(tx::base_symbols([&a, &b2], 1, 8)), rate, 0.5);
        add_noise(&mut iq, 0.03, 11);
        let (ev, _) = decode(&iq, rate);
        let ends: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                CallEvent::End { key, info, .. } => Some((*key, info.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 2, "{ev:?}");
        let ts1 = ends.iter().find(|(_, i)| i.timeslot == Some(1)).unwrap();
        let ts2 = ends.iter().find(|(_, i)| i.timeslot == Some(2)).unwrap();
        assert_eq!(ts1.1.lc.map(|l| (l.dst, l.src)), Some((91, 1_111_111)));
        assert_eq!(ts2.1.lc.map(|l| (l.dst, l.src)), Some((3153, 2_222_222)));
        assert_eq!(ts2.1.call_type(), Some("private"));
        assert!(ts1.1.repeater);
        assert_eq!(voice_frames(&ev, ts1.0).len(), 3 * 6 * 3);
        assert_eq!(voice_frames(&ev, ts2.0).len(), 2 * 6 * 3);
    }

    #[test]
    fn noise_and_fm_find_no_sync() {
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
